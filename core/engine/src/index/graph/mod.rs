//! Bounded graph relationships over the collection page-WAL.
//!
//! Primary edge rows are authoritative user data. Reverse rows are mandatory
//! navigation markers and cannot recover missing primary properties.
use crate::collections::{
    corrupt, invalid, ordered, ordered_into, packet, read_ordered, replicas, row_key, unpack,
    CollectionId, Database, EntityId, Error, IndexHeader, Result, PAD,
};
use crate::query::{QueryFilter, ScalarValue};
use crate::store::pagewal::PageWalStore;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) mod adjacency;
pub(crate) mod edge_table;
pub mod property_graph;
pub mod endpoints;

pub(crate) const GRAPH_FEATURE: u64 = 2;
pub(crate) const GRAPH_HEADER: u8 = 0x06;
pub(crate) const NAME_DESCRIPTOR: u8 = 0x07;
pub(crate) const NAME_LOOKUP: u8 = 0x12;
pub(crate) const PRIMARY_EDGE: u8 = 0x71;
pub(crate) const REVERSE_EDGE: u8 = 0x72;
/// The EDGE-ID ALLOCATOR: three replicas `0x09 | copy`, each
/// `packet(E4GEI01, next: u64 BE)`. Present exactly when the file declares
/// [`EDGE_ID_FEATURE`]; `next` is the smallest id never handed out.
///
/// THE TAG. `0x09` is the first free byte after the live row count (`0x08`)
/// in the registry audited in `docs/core/FORMAT_V2.md` "Extension boundary":
/// `0x00`-`0x05` header, layout, catalog and index registry replicas, `0x06`
/// `0x07` graph header and name descriptors, `0x08` row counts, `0x10`-`0x12`
/// names, `0x20` external keys, `0x40` rows, `0x60` vector sidecars and the
/// full `0x70`-`0x7F` index run.
pub(crate) const EDGE_ID_ALLOCATOR: u8 = 0x09;
/// The additive logical feature bit that says this file carries
/// id-bearing edge keys (`docs/core/GRAPH_CONTRACT.md` §2.3).
///
/// Set in the same transaction as the FIRST id-bearing key, never cleared.
/// A file that never calls [`Database::create_edge`] never declares it, and
/// is byte for byte what an older binary wrote. A binary whose mask predates
/// it refuses a file that declares it whole, at admission, as `Unsupported`
/// (Law 8) -- it never gets as far as meeting an id segment it would call a
/// malformed key.
pub const EDGE_ID_FEATURE: u64 = 0x40000;
/// The id every edge written through the TUPLE-keyed calls (`put_edge`,
/// `link`, `link_many`) carries, and every edge of a file without
/// [`EDGE_ID_FEATURE`]: the implicit identity, which is the tuple itself. Its
/// keys carry no id segment at all.
pub(crate) const IMPLICIT_EDGE_ID: u64 = 0;
const EDGE_ID_MAGIC: &[u8; 8] = b"E4GEI01\0";
/// How many edges of ONE tuple a tuple-keyed delete removes in one call. A
/// tuple's edges are one contiguous range and are collected before any of
/// them is deleted, so this bounds that list (Law 1).
const MAX_TUPLE_EDGES: usize = 65_536;
const GRAPH_MAGIC: &[u8; 8] = b"E4GRF01\0";
const NAME_MAGIC: &[u8; 8] = b"E4GNM01\0";
const GRAPH_ENCODING: u16 = 1;
const REVERSE_REQUIRED: u16 = 1;
const MAX_NAMES: u32 = 4096;
const MAX_NAME_BYTES: usize = 128;
const MAX_EDGE_PROPERTY_BYTES: usize = 64 * 1024;
const MAX_NEIGHBORS: usize = 256;
const MAX_NEIGHBOR_PROPERTY_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_BFS_DEPTH: usize = 64;
pub(crate) const MAX_BFS_VISITED: usize = 65_536;
pub(crate) const MAX_BFS_EDGES: usize = 1_000_000;
pub(crate) const MAX_BFS_RESULTS: usize = 65_536;
const MAX_CASCADE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeTypeId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphContextId(pub u64);
impl GraphContextId {
    pub const BASE: Self = Self(0);
}

/// The TUPLE of an edge: source, context, type, destination.
///
/// For the tuple-keyed calls (`put_edge`, `link`, `link_many`) the tuple IS
/// the identity: a second `put_edge` of the same quadruple overwrites the
/// first (set semantics), and every edge of a file written before
/// `docs/core/GRAPH_CONTRACT.md` §2.3 has this implicit identity (id 0).
/// [`Database::create_edge`] adds an id segment under
/// [`EDGE_ID_FEATURE`], so PARALLEL edges of one tuple coexist; [`EdgeId`]
/// is that tuple plus the id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeKey {
    pub source: EntityId,
    pub context: GraphContextId,
    pub edge_type: EdgeTypeId,
    pub destination: EntityId,
}

/// One edge's STABLE identity (`docs/core/GRAPH_CONTRACT.md` §2.3): its tuple
/// and its id.
///
/// `id` is allocated by [`Database::create_edge`] from one database-wide
/// counter that only moves forward, so it is unique across the whole file
/// and is never handed out twice, deleted edges included. The tuple rides
/// along because the id is the LAST segment of the edge key: with the tuple
/// in hand, reading, updating or deleting one edge is one point read, and
/// no id-to-tuple index is kept (identity costs the id segment and nothing
/// else). An edge written by a tuple-keyed call has
/// `id == 0`, which addresses the tuple's own edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeId {
    pub key: EdgeKey,
    pub id: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub key: EdgeKey,
    /// 0 (the implicit identity) for a tuple-keyed edge, otherwise the id
    /// [`Database::create_edge`] handed out.
    pub id: u64,
    pub properties: Value,
}

/// One row of the graph SHAPE: an edge type, the context it was written in,
/// and the two collections it connects. See [`Database::edge_shape`].
///
/// Ordered by `(context, edge type, from, to)`, which is the order the
/// deduplicating set produces and therefore the order a listing prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeShape {
    pub context: GraphContextId,
    pub edge_type: EdgeTypeId,
    pub from: CollectionId,
    pub to: CollectionId,
}

/// One edge of a [`Database::link_many`] batch: the two endpoints and the
/// property bag. The context and the edge type are the batch's, named once.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEdge {
    pub source: EntityId,
    pub destination: EntityId,
    pub properties: Value,
}

/// The edge a traversal crossed to reach one node, bound to that node
/// (`docs/core/GRAPH_CONTRACT.md` §4.2). `properties` is the inline bag of the
/// PRIMARY posting, decoded once when the edge was walked; nothing here
/// comes from a row.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeRef {
    pub key: EdgeKey,
    /// The edge's id: 0 (the implicit identity) for a tuple-keyed edge.
    pub id: u64,
    pub properties: Value,
}

/// The comparison one [`EdgePredicate`] makes.
///
/// `Ne` is here and is NOT the `<>` that `docs/lang/QL_CONTRACT.md` §3 refuses
/// for a scalar column. That refusal is about an INDEX: the complement of an
/// equality is not a posting range and so is not over a membership set. An
/// edge predicate reads the property out of the posting the hop is standing
/// on, so the complement costs exactly what the predicate costs and no set
/// is involved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// One per-hop predicate over an edge's inline property bag
/// (`docs/core/GRAPH_CONTRACT.md` §4.3). The conjunction of a request's
/// predicates decides whether the hop is followed; a failing edge is never
/// crossed and the node beyond it is never reached through it.
///
/// A property that is absent from the bag, or is JSON `null`, or holds
/// another JSON type than the predicate's value, satisfies NO comparison --
/// `Ne` included. That is the rule `scalar_filter_matches` already applies
/// to a missing row field, stated once for edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgePredicate<'a> {
    pub property: &'a str,
    pub op: Cmp,
    pub value: ScalarValue<'a>,
}

/// Does one decoded property bag satisfy every predicate?
pub(crate) fn edge_properties_match(properties: &Value, predicates: &[EdgePredicate<'_>]) -> bool {
    predicates
        .iter()
        .all(|predicate| edge_property_matches(properties, predicate))
}

fn edge_property_matches(properties: &Value, predicate: &EdgePredicate<'_>) -> bool {
    let Some(found) = properties.get(predicate.property) else {
        return false;
    };
    let ordering = match (found, predicate.value) {
        (Value::Bool(left), ScalarValue::Bool(right)) => left.cmp(&right),
        (Value::String(left), ScalarValue::Text(right)) => left.as_str().cmp(right),
        (Value::Number(left), ScalarValue::I64(right)) => match left.as_i64() {
            Some(left) => left.cmp(&right),
            None => match left.as_f64() {
                Some(left) => match left.partial_cmp(&(right as f64)) {
                    Some(ordering) => ordering,
                    None => return false,
                },
                None => return false,
            },
        },
        (Value::Number(left), ScalarValue::F64(right)) => {
            let Some(left) = left.as_f64() else {
                return false;
            };
            match left.partial_cmp(&right) {
                Some(ordering) => ordering,
                None => return false,
            }
        }
        _ => return false,
    };
    match predicate.op {
        Cmp::Eq => ordering == std::cmp::Ordering::Equal,
        Cmp::Ne => ordering != std::cmp::Ordering::Equal,
        Cmp::Lt => ordering == std::cmp::Ordering::Less,
        Cmp::Le => ordering != std::cmp::Ordering::Greater,
        Cmp::Gt => ordering == std::cmp::Ordering::Greater,
        Cmp::Ge => ordering != std::cmp::Ordering::Less,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Clone, Copy, Debug)]
pub struct NeighborRequest {
    pub entity: EntityId,
    pub direction: Direction,
    pub context: GraphContextId,
    pub edge_type: Option<EdgeTypeId>,
    /// Complete-or-error bound, in 0..=256. A result is never silently cut.
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BfsRequest<'a> {
    pub seed: EntityId,
    pub direction: Direction,
    pub context: GraphContextId,
    pub edge_type: Option<EdgeTypeId>,
    pub min_depth: usize,
    pub max_depth: usize,
    pub include_seed: bool,
    pub max_visited: usize,
    pub max_edges: usize,
    pub result_limit: usize,
    /// The conjunction every edge must satisfy to be followed
    /// (`docs/core/GRAPH_CONTRACT.md` §4.3). Decoded from the edge's inline bag as
    /// the frontier expands; a failing edge is not crossed. The same set
    /// applies to every hop (§4.4).
    pub edge_where: &'a [EdgePredicate<'a>],
    /// The conjunction every node REACHED BY AN EDGE must satisfy to be
    /// emitted and to be expanded (`docs/core/GRAPH_CONTRACT.md` §4.3). Restricted
    /// to the filter kinds an index answers without a row: a scalar equality
    /// (one posting probe per node) and a scalar range or a point predicate
    /// (one membership set, built once). Any other kind is refused when the
    /// traversal is prepared.
    ///
    /// The SEED is not tested: it is NAMED by the caller, not found by a hop,
    /// and §4.3's rule is about the frontier a hop produces.
    pub node_where: &'a [QueryFilter<'a>],
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraversalNode {
    pub entity: EntityId,
    pub depth: usize,
    /// The edge this traversal crossed to reach `entity`
    /// (`docs/core/GRAPH_CONTRACT.md` §4.2), or `None` for the seed and for a
    /// traversal that was not asked to bind it.
    ///
    /// A node reachable over several edges reports the FIRST one the walk
    /// admitted: edges are offered to a level in the order the postings are
    /// walked (outgoing before incoming, each in key order), and the level
    /// keeps the first offer per entity.
    pub via: Option<EdgeRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraversalResult {
    pub nodes: Vec<TraversalNode>,
    pub visited: usize,
    pub scanned_edges: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GraphHeader {
    pub(crate) next_type: u64,
    pub(crate) next_context: u64,
    pub(crate) type_count: u32,
    pub(crate) context_count: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GraphName {
    pub(crate) kind: u8,
    pub(crate) id: u64,
    pub(crate) name: String,
}

pub(crate) fn graph_header_key(copy: u8) -> Vec<u8> {
    vec![GRAPH_HEADER, copy]
}

pub(crate) fn name_descriptor_key(kind: u8, copy: u8, id: u64) -> Vec<u8> {
    let mut key = vec![NAME_DESCRIPTOR, kind, copy];
    key.extend(ordered(id));
    key
}

pub(crate) fn name_lookup_key(kind: u8, name: &str) -> Vec<u8> {
    let mut key = vec![NAME_LOOKUP, kind];
    key.extend_from_slice(name.as_bytes());
    key
}

/// The base graph's encoding and flags, as a Register file's `GRPH`
/// payload holds them (`docs/core/SUPPORTIVE.md` 2.e).
pub(crate) fn graph_flags() -> Vec<u8> {
    let mut b = GRAPH_ENCODING.to_be_bytes().to_vec();
    b.extend(REVERSE_REQUIRED.to_be_bytes());
    b
}

fn encode_graph_header(h: GraphHeader) -> Result<Vec<u8>> {
    let mut body = GRAPH_ENCODING.to_be_bytes().to_vec();
    body.extend(REVERSE_REQUIRED.to_be_bytes());
    body.extend(h.next_type.to_be_bytes());
    body.extend(h.next_context.to_be_bytes());
    body.extend(h.type_count.to_be_bytes());
    body.extend(h.context_count.to_be_bytes());
    packet(GRAPH_MAGIC, &body)
}

pub(crate) fn decode_graph_header(bytes: &[u8]) -> Result<GraphHeader> {
    if bytes.len() == PAD && bytes.starts_with(b"E4GRF") && &bytes[..8] != GRAPH_MAGIC {
        unpack(bytes, bytes[..8].try_into().unwrap())?;
        return Err(Error::Unsupported("graph header encoding".into()));
    }
    let body = unpack(bytes, GRAPH_MAGIC)?;
    if body.len() != 28 {
        return Err(Error::Unsupported(format!(
            "graph header payload of {} bytes",
            body.len()
        )));
    }
    let encoding = u16::from_be_bytes(body[..2].try_into().unwrap());
    let flags = u16::from_be_bytes(body[2..4].try_into().unwrap());
    if encoding != GRAPH_ENCODING || flags != REVERSE_REQUIRED {
        return Err(Error::Unsupported(format!(
            "graph encoding {encoding} flags {flags:#x}"
        )));
    }
    let h = GraphHeader {
        next_type: u64::from_be_bytes(body[4..12].try_into().unwrap()),
        next_context: u64::from_be_bytes(body[12..20].try_into().unwrap()),
        type_count: u32::from_be_bytes(body[20..24].try_into().unwrap()),
        context_count: u32::from_be_bytes(body[24..28].try_into().unwrap()),
    };
    if h.next_type == 0
        || h.next_context == 0
        || h.type_count > MAX_NAMES
        || h.context_count > MAX_NAMES
        || h.next_type != u64::from(h.type_count) + 1
        || h.next_context != u64::from(h.context_count) + 1
    {
        return Err(corrupt("graph name allocator/count"));
    }
    Ok(h)
}

pub(crate) fn read_graph_header(
    get: impl FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<GraphHeader> {
    replicas(get, graph_header_key, decode_graph_header)
}

pub(crate) fn edge_id_allocator_key(copy: u8) -> Vec<u8> {
    vec![EDGE_ID_ALLOCATOR, copy]
}

pub(crate) fn encode_edge_id_allocator(next: u64) -> Result<Vec<u8>> {
    packet(EDGE_ID_MAGIC, &next.to_be_bytes())
}

/// One allocator replica. An intact packet of a NEWER encoding is
/// `Unsupported`, exactly as the graph header's is; `next` is at least 1,
/// because id 0 is the implicit identity and is never allocated.
pub(crate) fn decode_edge_id_allocator(bytes: &[u8]) -> Result<u64> {
    if bytes.len() == PAD && bytes.starts_with(b"E4GEI") && &bytes[..8] != EDGE_ID_MAGIC {
        unpack(bytes, bytes[..8].try_into().unwrap())?;
        return Err(Error::Unsupported("graph edge-id allocator encoding".into()));
    }
    let body = unpack(bytes, EDGE_ID_MAGIC)?;
    if body.len() != 8 {
        return Err(corrupt("graph edge-id allocator length"));
    }
    let next = u64::from_be_bytes(body[..8].try_into().unwrap());
    if next == 0 {
        return Err(corrupt("graph edge-id allocator is zero"));
    }
    Ok(next)
}

pub(crate) fn read_edge_id_allocator(
    get: impl FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<u64> {
    replicas(get, edge_id_allocator_key, decode_edge_id_allocator)
}

fn encode_name(n: &GraphName) -> Result<Vec<u8>> {
    let mut body = vec![n.kind];
    body.extend(n.id.to_be_bytes());
    body.extend((n.name.len() as u16).to_be_bytes());
    body.extend_from_slice(n.name.as_bytes());
    packet(NAME_MAGIC, &body)
}

pub(crate) fn decode_name(bytes: &[u8]) -> Result<GraphName> {
    if bytes.len() == PAD && bytes.starts_with(b"E4GNM") && &bytes[..8] != NAME_MAGIC {
        unpack(bytes, bytes[..8].try_into().unwrap())?;
        return Err(Error::Unsupported("graph name descriptor encoding".into()));
    }
    let body = unpack(bytes, NAME_MAGIC)?;
    if body.len() < 12 {
        return Err(corrupt("short graph name descriptor"));
    }
    let kind = body[0];
    let id = u64::from_be_bytes(body[1..9].try_into().unwrap());
    let len = u16::from_be_bytes(body[9..11].try_into().unwrap()) as usize;
    if kind > 1 || id == 0 || len == 0 || len > MAX_NAME_BYTES || body.len() != 11 + len {
        return Err(corrupt("graph name descriptor fields"));
    }
    let name = std::str::from_utf8(&body[11..])
        .map_err(corrupt)?
        .to_owned();
    Ok(GraphName { kind, id, name })
}

pub(crate) fn read_name(
    get: impl FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
    kind: u8,
    id: u64,
) -> Result<GraphName> {
    replicas(
        get,
        |copy| name_descriptor_key(kind, copy, id),
        |bytes| {
            let n = decode_name(bytes)?;
            if n.kind != kind || n.id != id {
                return Err(corrupt("graph name descriptor identity"));
            }
            Ok(n)
        },
    )
}

fn append_entity(key: &mut Vec<u8>, id: EntityId) {
    ordered_into(key, id.collection.0.into());
    ordered_into(key, id.sequence);
}

/// Room for the widest edge key: a tag, two identities at their widest, the
/// context and type identities, and the optional edge id segment. Sized once
/// so building a key is one allocation rather than one per integer in it
/// plus a regrow per component.
const EDGE_KEY_BYTES: usize = 1 + 2 * (2 + 9) + 9 + 9 + 9;

/// The smallest key that sorts ABOVE every key carrying `prefix`: the byte
/// successor of the prefix. `None` when every byte is `0xFF` and there is no
/// such key, which means the prefix reaches the end of the keyspace.
fn key_after_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == u8::MAX {
            out.pop();
            continue;
        }
        *last += 1;
        return Some(out);
    }
    None
}

fn read_entity(key: &[u8], at: &mut usize) -> Result<EntityId> {
    let collection = u32::try_from(read_ordered(key, at)?).map_err(corrupt)?;
    let sequence = read_ordered(key, at)?;
    if collection == 0 || sequence == 0 {
        return Err(corrupt("graph entity identity"));
    }
    Ok(EntityId {
        collection: CollectionId(collection),
        sequence,
    })
}

/// DIAGNOSTIC: per-stage cost of writing one relationship, in nanoseconds and
/// in buffer-pool page accesses, accumulated over a run of
/// `Database::put_edge_measured`. Nothing on the shipping write path touches
/// it; `bench/src/bin/g2_budget.rs` is its only caller.
#[doc(hidden)]
#[derive(Default, Clone, Copy, Debug)]
pub struct EdgeBudget {
    pub edges: u64,
    /// Endpoints (of 2 per edge) whose row existence was already known.
    pub fast_endpoints: u64,
    /// Edges whose forward/reverse probe was provably pointless.
    pub fast_preflight: u64,
    pub header_ns: u64,
    pub header_pages: u64,
    pub endpoints_ns: u64,
    pub endpoint_pages: u64,
    pub encode_ns: u64,
    pub encode_pages: u64,
    pub preflight_ns: u64,
    pub preflight_pages: u64,
    pub keybuild_ns: u64,
    pub keybuild_pages: u64,
    pub put_primary_ns: u64,
    pub put_primary_pages: u64,
    pub put_reverse_ns: u64,
    pub put_reverse_pages: u64,
    pub finish_ns: u64,
    pub finish_pages: u64,
    /// The ENDPOINT SET keys of a NEW edge: two puts, or nothing at all when
    /// the edge was already on disk or the file carries no sets
    /// (`index/graph/endpoints.rs`).
    pub endpoint_keys_ns: u64,
    pub endpoint_keys_pages: u64,
}

impl EdgeBudget {
    /// `(label, nanoseconds, page accesses)` for every stage, in the order
    /// `put_edge` runs them.
    pub fn stages(&self) -> [(&'static str, u64, u64); 9] {
        [
            ("header+ids", self.header_ns, self.header_pages),
            ("endpoints", self.endpoints_ns, self.endpoint_pages),
            ("encode props", self.encode_ns, self.encode_pages),
            ("preflight", self.preflight_ns, self.preflight_pages),
            ("key build", self.keybuild_ns, self.keybuild_pages),
            ("put 0x71", self.put_primary_ns, self.put_primary_pages),
            ("put 0x72", self.put_reverse_ns, self.put_reverse_pages),
            ("put 0x7E", self.endpoint_keys_ns, self.endpoint_keys_pages),
            ("finish", self.finish_ns, self.finish_pages),
        ]
    }
    pub fn total_ns(&self) -> u64 {
        self.stages().iter().map(|s| s.1).sum()
    }
    pub fn total_pages(&self) -> u64 {
        self.stages().iter().map(|s| s.2).sum()
    }
}

/// The key of a TUPLE-keyed edge (id [`IMPLICIT_EDGE_ID`]).
pub(crate) fn edge_key(tag: u8, edge: EdgeKey) -> Vec<u8> {
    edge_key_id(tag, edge, IMPLICIT_EDGE_ID)
}

/// The key of one edge of either kind.
///
/// ```text
/// 0x71 | source | context | type | destination [| ordered(id)]
/// 0x72 | destination | context | type | source [| ordered(id)]
/// ```
///
/// The id segment is ABSENT for [`IMPLICIT_EDGE_ID`], so a tuple-keyed edge
/// keeps the frozen encoding byte for byte, and PRESENT (2..=9 bytes) for an
/// id-bearing edge. Every field before it is self-delimiting (the ordered
/// integer encoding is width-tagged), so the tuple key is a byte prefix of
/// exactly its own parallel edges and of nothing else, and one adjacency
/// prefix still covers every parallel edge in one contiguous range.
pub(crate) fn edge_key_id(tag: u8, edge: EdgeKey, id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(EDGE_KEY_BYTES);
    edge_key_into_id(&mut key, tag, edge, id);
    key
}

/// The same frozen encoding, appended to a buffer the caller owns.
pub(super) fn edge_key_into(key: &mut Vec<u8>, tag: u8, edge: EdgeKey) {
    edge_key_into_id(key, tag, edge, IMPLICIT_EDGE_ID)
}

fn edge_key_into_id(key: &mut Vec<u8>, tag: u8, edge: EdgeKey, id: u64) {
    key.push(tag);
    let (first, last) = if tag == PRIMARY_EDGE {
        (edge.source, edge.destination)
    } else {
        (edge.destination, edge.source)
    };
    append_entity(key, first);
    ordered_into(key, edge.context.0);
    ordered_into(key, edge.edge_type.0);
    append_entity(key, last);
    if id != IMPLICIT_EDGE_ID {
        ordered_into(key, id);
    }
}

/// The optional id segment at `key[*at..]`: [`IMPLICIT_EDGE_ID`] when the
/// key ends here, otherwise one ordered integer that must be nonzero and end
/// the key. Anything else is a malformed key.
#[inline]
fn read_edge_id(key: &[u8], at: &mut usize) -> Result<u64> {
    if *at == key.len() {
        return Ok(IMPLICIT_EDGE_ID);
    }
    let id = read_ordered(key, at)?;
    if id == IMPLICIT_EDGE_ID || *at != key.len() {
        return Err(corrupt("graph edge id segment"));
    }
    Ok(id)
}

pub(super) fn edge_prefix(
    tag: u8,
    entity: EntityId,
    context: Option<GraphContextId>,
    edge_type_id: Option<EdgeTypeId>,
) -> Vec<u8> {
    let mut key = vec![tag];
    append_entity(&mut key, entity);
    if let Some(context) = context {
        key.extend(ordered(context.0));
        if let Some(edge_type_id) = edge_type_id {
            key.extend(ordered(edge_type_id.0));
        }
    }
    key
}

/// The widest an edge-scan prefix can be: the tag, two integers for the near
/// entity, one for the context and one for the type, each at the nine bytes
/// the ordered encoding uses for a full `u64`.
pub(crate) const MAX_EDGE_PREFIX: usize = 1 + 9 + 9 + 9 + 9;

/// The same prefix, written into the caller's stack buffer.
///
/// A one-hop read is a few hundred nanoseconds of real work, and it built this
/// eleven-byte key with a heap allocation -- once per direction, on every call.
/// The encoding is the frozen one; the debug assertion below holds it to
/// `edge_prefix` byte for byte, so the two can never drift apart unnoticed.
pub(crate) fn edge_prefix_into(
    buf: &mut [u8; MAX_EDGE_PREFIX],
    tag: u8,
    entity: EntityId,
    context: Option<GraphContextId>,
    edge_type_id: Option<EdgeTypeId>,
) -> usize {
    fn put(buf: &mut [u8; MAX_EDGE_PREFIX], at: &mut usize, n: u64) {
        let bytes = n.to_be_bytes();
        let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
        buf[*at] = 0x80 + (8 - start) as u8;
        *at += 1;
        buf[*at..*at + 8 - start].copy_from_slice(&bytes[start..]);
        *at += 8 - start;
    }
    let mut at = 0;
    buf[at] = tag;
    at += 1;
    put(buf, &mut at, entity.collection.0.into());
    put(buf, &mut at, entity.sequence);
    if let Some(context) = context {
        put(buf, &mut at, context.0);
        if let Some(edge_type_id) = edge_type_id {
            put(buf, &mut at, edge_type_id.0);
        }
    }
    debug_assert_eq!(
        &buf[..at],
        edge_prefix(tag, entity, context, edge_type_id).as_slice(),
        "the stack prefix encoder drifted from the frozen edge-key encoding"
    );
    at
}

/// Both halves of a stored edge key: the tuple and the id
/// ([`IMPLICIT_EDGE_ID`] when the key carries no id segment).
///
/// Whether an id segment is ALLOWED is the file's business, not the key's:
/// the verifier and the rebuild refuse one in a file that does not declare
/// [`EDGE_ID_FEATURE`], and a binary that predates the bit never opens such
/// a file at all.
pub(crate) fn parse_edge(key: &[u8], tag: u8) -> Result<(EdgeKey, u64)> {
    if key.first() != Some(&tag) {
        return Err(corrupt("graph edge key tag"));
    }
    let mut at = 1;
    let first = read_entity(key, &mut at)?;
    let context = GraphContextId(read_ordered(key, &mut at)?);
    let edge_type = EdgeTypeId(read_ordered(key, &mut at)?);
    let last = read_entity(key, &mut at)?;
    if edge_type.0 == 0 {
        return Err(corrupt("graph edge key fields"));
    }
    let id = read_edge_id(key, &mut at).map_err(|_| corrupt("graph edge key fields"))?;
    let edge = if tag == PRIMARY_EDGE {
        EdgeKey {
            source: first,
            context,
            edge_type,
            destination: last,
        }
    } else {
        EdgeKey {
            source: last,
            context,
            edge_type,
            destination: first,
        }
    };
    Ok((edge, id))
}

/// The far endpoint of an edge key that a prefix scan just handed us.
///
/// `parse_edge` reads all four fields back out of the key. A prefix scan
/// has already fixed three of them -- the near entity, the context, and, when
/// the caller named one, the edge type -- byte for byte: the `starts_with`
/// test proved the row carries exactly the bytes the scan built. Only the far
/// entity is news, and for a traversal it is the whole answer.
///
/// Measured on the 500-edge organization fan-in of the 50K multimodel
/// database: 32.7 ns per edge for `parse_edge`, 12.0 ns for this.
pub(crate) fn adjacent_from_tail(
    key: &[u8],
    at0: usize,
    pinned_type: Option<EdgeTypeId>,
    context: GraphContextId,
    h: GraphHeader,
) -> Result<(EdgeTypeId, EntityId, u64)> {
    let mut at = at0;
    let edge_type = match pinned_type {
        Some(id) => id,
        None => EdgeTypeId(read_ordered(key, &mut at)?),
    };
    let adjacent = read_entity(key, &mut at)?;
    // The id segment of a parallel edge, or nothing: one length comparison
    // per hop for a tuple-keyed edge, one integer read for an id-bearing one,
    // and no allocation either way (contract §2.3: nothing per hop).
    let id = read_edge_id(key, &mut at).map_err(|_| corrupt("graph edge key fields"))?;
    // The guard `validate_stored_edge_ids` applied, on the same two fields.
    // The context reached us through the prefix the caller already validated;
    // the type came either from there or from the key we just read.
    if edge_type.0 == 0 || edge_type.0 >= h.next_type || context.0 >= h.next_context {
        return Err(corrupt("stored edge has unknown type/context identity"));
    }
    Ok((edge_type, adjacent, id))
}

/// The encoded form of `{}`. Nearly every edge in a graph carries no
/// properties at all, and decoding that costs a reader run and a map. The
/// encoder is the authority on these three bytes and `encode_properties`
/// asserts they stay in step.
const EMPTY_PROPERTIES: &[u8] = &[1, 8, 0];

fn encode_properties(value: &Value) -> Result<Vec<u8>> {
    let Some(object) = value.as_object() else {
        return Err(invalid("edge properties must be an object"));
    };
    // The overwhelmingly common edge carries no properties at all. Running the
    // binary-JSON writer over an empty map to rediscover three frozen bytes is
    // two allocations for a constant; `decode_properties` already short-cuts
    // the same three, and `empty_properties_are_what_the_writer_writes` keeps
    // the pair honest -- once, in a test, not on every edge of a debug build.
    if object.is_empty() {
        return Ok(EMPTY_PROPERTIES.to_vec());
    }
    let binary = crate::binary_json(value).map_err(invalid)?;
    if binary.len() + 1 > MAX_EDGE_PROPERTY_BYTES {
        return Err(invalid("encoded edge properties exceed 64 KiB"));
    }
    let mut out = vec![1];
    out.extend(binary);
    debug_assert!(
        !value.as_object().is_some_and(serde_json::Map::is_empty) || out == EMPTY_PROPERTIES,
        "the empty-object encoding moved out from under decode_properties"
    );
    Ok(out)
}

/// A stored bag of an edge of type `t`, under its table's column names
/// (F1 on edges, `crate::collections::columns::BagMap`).
pub(crate) fn named_bag(db: &Database, t: EdgeTypeId, bag: Value) -> Result<Value> {
    Ok(match db.bag_map(t)? {
        Some(m) => m.names(&bag),
        None => bag,
    })
}

pub(crate) fn decode_properties(bytes: &[u8]) -> Result<Value> {
    if bytes.first() != Some(&1) || bytes.len() > MAX_EDGE_PROPERTY_BYTES {
        return Err(corrupt("edge property encoding/size"));
    }
    if bytes == EMPTY_PROPERTIES {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let value = crate::read_binary_json(&bytes[1..]).map_err(corrupt)?;
    if !value.is_object() {
        return Err(corrupt("edge properties are not an object"));
    }
    Ok(value)
}

/// The next BFS level. A `BTreeSet` allocated a tree node per edge to answer
/// a question the end of the level answers anyway; this keeps the candidates
/// in a vector and sorts once, which is also where the deterministic order
/// comes from. The visited limit still trips at exactly the count it tripped
/// at before: `offer` sorts and dedups the moment the raw count could exceed
/// the room the level has left, and a level that is full answers membership
/// by binary search over the sorted vector.
pub(crate) struct Frontier {
    items: Vec<FrontierEntry>,
    sorted: bool,
    room: usize,
    /// How many offers this level has seen, so the FIRST offer of an entity
    /// is recoverable after an unstable sort. §4.2's "a node reached by
    /// several edges reports the first admitted" is this counter and nothing
    /// else.
    offered: u32,
}

/// One entity a level discovered, and the edge that reached it.
///
/// Named sacrifice (Law 4): a level entry is 8 bytes of reaching-edge
/// pointer and 4 of offer order wider than the bare `EntityId` it used to
/// be. The `Arc` is shared with the level's other holders rather than cloned
/// per node, it is `None` for every traversal that does not bind the edge,
/// and the visited budget bounds the count exactly as before (Law 1).
#[derive(Clone)]
pub(crate) struct FrontierEntry {
    pub(crate) entity: EntityId,
    offer: u32,
    pub(crate) via: Option<Arc<EdgeRef>>,
}

impl Frontier {
    pub(crate) fn new(room: usize) -> Self {
        Self {
            items: Vec::new(),
            sorted: true,
            room,
            offered: 0,
        }
    }

    pub(crate) fn offer(&mut self, entity: EntityId, via: Option<Arc<EdgeRef>>) -> Result<()> {
        let offer = self.offered;
        self.offered = self.offered.saturating_add(1);
        if self.sorted && self.items.len() == self.room {
            // Full and normalised: one more distinct entity is one too many.
            return if self
                .items
                .binary_search_by_key(&entity, |item| item.entity)
                .is_ok()
            {
                Ok(())
            } else {
                Err(invalid("BFS visited limit exceeded"))
            };
        }
        self.items.push(FrontierEntry { entity, offer, via });
        self.sorted = false;
        if self.items.len() > self.room {
            self.normalize();
            if self.items.len() > self.room {
                return Err(invalid("BFS visited limit exceeded"));
            }
        }
        Ok(())
    }

    fn normalize(&mut self) {
        if !self.sorted {
            // `(entity, offer)` is a total order, so an unstable sort is
            // deterministic and the first entry of each entity group is the
            // first offer -- the edge §4.2 reports.
            self.items
                .sort_unstable_by_key(|item| (item.entity, item.offer));
            self.items.dedup_by_key(|item| item.entity);
            self.sorted = true;
        }
    }

    pub(crate) fn into_sorted(mut self) -> Vec<FrontierEntry> {
        self.normalize();
        self.items
    }
}

/// One hop of the node-deduplicating BFS -- `traverse_bfs` and the query
/// engine's `QueryFilter::Graph` walk run the same one -- over the edges of
/// one context and (optionally) one type, under the per-hop predicates of
/// `docs/core/GRAPH_CONTRACT.md` §4.3, applied HERE, as the frontier
/// expands, and not afterwards:
///
///   * `edge_where` reads the edge's own inline property bag. An OUTGOING
///     posting carries that bag as its value, so the predicate costs the
///     decode and nothing else. An INCOMING posting is a marker whose value
///     is empty by construction, so the authoritative primary posting of the
///     same edge is read back -- one edge-keyspace point read per candidate
///     edge, never a row. Those reads are made AFTER the range walk, from a
///     list the walk collected, because the walk holds a pinned leaf while
///     it runs.
///   * the node gate (the `node_where` conjunction) is tested on the far
///     endpoint ([`HopWork::admits`]). A node it refuses is neither offered
///     to the level nor expanded, and nothing about it is read from the
///     primary tree.
///
/// `wants_via` asks for the reaching edge to be bound to the node (§4.2).
/// It costs the same decode `edge_where` already pays, and for an incoming
/// hop the same primary-posting read.
pub(crate) struct Hop<'a> {
    pub(crate) header: GraphHeader,
    pub(crate) context: GraphContextId,
    pub(crate) edge_type: Option<EdgeTypeId>,
    pub(crate) edge_where: &'a [EdgePredicate<'a>],
    pub(crate) wants_via: bool,
    /// Edge-keyspace reads one walk may make, postings and primary-posting
    /// reads together.
    pub(crate) max_edges: usize,
}

/// What a [`Hop`]'s caller decides: when it polls for cancellation, what it
/// charges, and its node gate. Everything else about the hop is one code.
pub(crate) trait HopWork {
    type Error: From<Error>;
    /// One turn of the adjacency walk, which `found` a posting or the
    /// range's end.
    fn turn(&mut self, found: bool) -> std::result::Result<(), Self::Error>;
    /// Before one deferred incoming edge is considered.
    fn poll(&mut self) -> std::result::Result<(), Self::Error>;
    /// One primary-posting read, for a deferred incoming edge's bag.
    fn primary_read(&mut self) -> std::result::Result<(), Self::Error>;
    /// Whether the walk has a node gate at all.
    fn gated(&self) -> bool;
    /// The gate's own answer for `id`: its index point reads.
    fn probe(&mut self, id: EntityId) -> std::result::Result<bool, Self::Error>;
    /// Every entity the gate has turned away in this walk.
    fn refused(&mut self) -> &mut BTreeSet<EntityId>;

    /// The node gate, asked at most once per distinct entity for a whole
    /// walk.
    ///
    /// `docs/core/GRAPH_CONTRACT.md` §4.3 promises one index point read per
    /// visited NODE. The gate itself is per-node -- the same entity gets the
    /// same answer over every edge that reaches it -- so memoising the
    /// refusals changes no answer and no offer order: an admitted node joins
    /// `seen` with its level and is never offered again, and a refused one
    /// joins `refused` here instead of being re-probed by every incident
    /// edge at every later depth.
    ///
    /// Bound (Law 1): a refused entity was reached by at least one edge the
    /// walk charged against `max_edges`, so `refused` holds at most
    /// `max_edges` entries -- 12 bytes each, beside the visited set's own
    /// bound.
    fn admits(&mut self, id: EntityId) -> std::result::Result<bool, Self::Error> {
        if !self.gated() {
            return Ok(true);
        }
        if self.refused().contains(&id) {
            return Ok(false);
        }
        if self.probe(id)? {
            return Ok(true);
        }
        self.refused().insert(id);
        Ok(false)
    }
}

impl Hop<'_> {
    /// The hop from `entity` in `direction` (`Outgoing` or `Incoming`):
    /// every far node not in `seen` (sorted) that the predicates admit is
    /// offered to `next`, with its reaching edge when `wants_via`. Counts
    /// every edge-keyspace read into `scanned` against `max_edges`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn walk<W: HopWork>(
        &self,
        db: &Database,
        entity: EntityId,
        direction: Direction,
        seen: &[EntityId],
        next: &mut Frontier,
        scanned: &mut usize,
        work: &mut W,
    ) -> std::result::Result<(), W::Error> {
        // Whether this hop has to look at an edge's properties at all.
        let reads_properties = self.wants_via || !self.edge_where.is_empty();
        // Incoming edges whose properties this hop still needs. Bounded by
        // the edge budget the walk below trips on, so it is not a new bound.
        let mut deferred: Vec<(EdgeKey, u64, EntityId)> = Vec::new();
        // The shared adjacency walk (`adjacency.rs`) hands out borrows into
        // the pinned leaf: a traversal is a scan, and it costs the pages.
        let mut cursor = adjacency::AdjacencyCursor::at(
            db,
            self.header,
            entity,
            direction,
            self.context,
            self.edge_type,
        )?;
        loop {
            let posting = cursor.next_posting()?;
            work.turn(posting.is_some())?;
            let Some(posting) = posting else {
                break;
            };
            self.count(scanned)?;
            let adjacent = posting.edge()?;
            // The visited set is a SORTED VECTOR and the level being built
            // is a `Frontier`: a binary search, not a tree insert, per edge.
            if seen.binary_search(&adjacent.far).is_ok() {
                continue;
            }
            if !reads_properties {
                if work.admits(adjacent.far)? {
                    next.offer(adjacent.far, None)?;
                }
                continue;
            }
            // An incoming posting is a marker: its bag is read below.
            let Some(properties) = adjacent.bag()? else {
                deferred.push((adjacent.key, adjacent.id, adjacent.far));
                continue;
            };
            let properties = named_bag(db, adjacent.key.edge_type, properties)?;
            self.follow(properties, adjacent.key, adjacent.id, adjacent.far, next, work)?;
        }
        // The walk holds a pinned leaf; the reads below do not share it.
        drop(cursor);
        // The incoming hop's second pass: the reverse posting is a marker, so
        // the properties come from the primary posting of the same edge.
        for (edge, id, far) in deferred {
            work.poll()?;
            if seen.binary_search(&far).is_ok() {
                continue;
            }
            // The primary-posting read is an edge-keyspace read like the
            // range walk's own, so `max_edges` bounds it and `scanned`
            // reports it. It is counted HERE rather than at the top of the
            // loop because the `seen` test above skips the read entirely,
            // and counting a read that does not happen would report work
            // nobody did.
            self.count(scanned)?;
            work.primary_read()?;
            let properties = decode_properties(&adjacency::primary_posting(db, edge, id)?)?;
            let properties = named_bag(db, edge.edge_type, properties)?;
            self.follow(properties, edge, id, far, next, work)?;
        }
        Ok(())
    }

    /// Cross an edge whose bag is in hand: offer its far node when the
    /// edge's predicates and the node gate admit it.
    fn follow<W: HopWork>(
        &self,
        properties: Value,
        key: EdgeKey,
        id: u64,
        far: EntityId,
        next: &mut Frontier,
        work: &mut W,
    ) -> std::result::Result<(), W::Error> {
        if !edge_properties_match(&properties, self.edge_where) || !work.admits(far)? {
            return Ok(());
        }
        let via = self
            .wants_via
            .then(|| Arc::new(EdgeRef { key, id, properties }));
        next.offer(far, via)?;
        Ok(())
    }

    /// One more edge-keyspace read, within `max_edges`.
    fn count(&self, scanned: &mut usize) -> Result<()> {
        *scanned = scanned
            .checked_add(1)
            .ok_or_else(|| invalid("BFS edge work overflow"))?;
        if *scanned > self.max_edges {
            return Err(invalid("BFS edge work limit exceeded"));
        }
        Ok(())
    }
}

/// `traverse_bfs`'s side of a [`Hop`]: its cancel callback, polled once per
/// posting and per deferred edge, and its standalone node gate.
struct BfsWork<'a, C> {
    db: &'a Database,
    gate: &'a crate::query::StandaloneNodeGate,
    refused: BTreeSet<EntityId>,
    cancel: C,
}

impl<C: FnMut() -> bool> HopWork for BfsWork<'_, C> {
    type Error = Error;

    fn turn(&mut self, found: bool) -> Result<()> {
        if found {
            self.poll()?;
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<()> {
        poll(&mut self.cancel)
    }

    fn primary_read(&mut self) -> Result<()> {
        Ok(())
    }

    fn gated(&self) -> bool {
        !self.gate.is_empty()
    }

    fn probe(&mut self, id: EntityId) -> Result<bool> {
        self.gate.admits(self.db, id)
    }

    fn refused(&mut self) -> &mut BTreeSet<EntityId> {
        &mut self.refused
    }
}

/// A graph read's cancel poll.
fn poll(cancel: &mut impl FnMut() -> bool) -> Result<()> {
    if cancel() {
        return Err(invalid("graph query cancelled"));
    }
    Ok(())
}

/// The poll for the row a finished walk read past its range, if it read one.
fn poll_past(cursor: &adjacency::AdjacencyCursor<'_>, cancel: &mut impl FnMut() -> bool) -> Result<()> {
    if cursor.read_past_range() {
        poll(cancel)?;
    }
    Ok(())
}

/// Union of two sorted sets with no element in common, in one pass, reusing
/// the caller's scratch buffer so a deep traversal allocates per LEVEL rather
/// than per entity.
pub(crate) fn merge_sorted_disjoint(
    seen: &mut Vec<EntityId>,
    next: &[EntityId],
    scratch: &mut Vec<EntityId>,
) {
    if next.is_empty() {
        return;
    }
    scratch.clear();
    scratch.reserve(seen.len() + next.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < seen.len() && j < next.len() {
        if seen[i] < next[j] {
            scratch.push(seen[i]);
            i += 1;
        } else {
            scratch.push(next[j]);
            j += 1;
        }
    }
    scratch.extend_from_slice(&seen[i..]);
    scratch.extend_from_slice(&next[j..]);
    std::mem::swap(seen, scratch);
}

/// The open-time admission of the edge-id allocator: bounded, three keys at
/// most. Without the bit no `0x09` key may exist; with it the graph must be
/// on and an intact replica must be readable, and an intact replica of a
/// newer encoding is refused as `Unsupported` rather than outvoted.
fn validate_edge_id_allocator(s: &PageWalStore, graph: bool, enabled: bool) -> Result<()> {
    if !enabled {
        if let Some(row) = s.range(&[EDGE_ID_ALLOCATOR])?.next() {
            let (key, _) = row?;
            if key.first() == Some(&EDGE_ID_ALLOCATOR) {
                return Err(corrupt(
                    "graph edge-id allocator exists without the edge identity feature",
                ));
            }
        }
        return Ok(());
    }
    if !graph {
        return Err(corrupt("edge identity feature declared without the graph feature"));
    }
    for row in s.range(&[EDGE_ID_ALLOCATOR])? {
        let (key, value) = row?;
        if key.first() != Some(&EDGE_ID_ALLOCATOR) {
            break;
        }
        if key.len() != 2 || key[1] > 2 {
            return Err(corrupt("graph edge-id allocator replica key"));
        }
        if let Err(e @ Error::Unsupported(_)) = decode_edge_id_allocator(&value) {
            return Err(e);
        }
    }
    read_edge_id_allocator(|key| s.get(key).map_err(Error::from))?;
    Ok(())
}

fn validate_name_input(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(invalid(format!("{what} requires 1..128 UTF-8 bytes")));
    }
    Ok(())
}

/// Bounded admission of graph metadata. Relationship rows deliberately are
/// not scanned here; their exact key/value and mirror are checked when read.
pub(crate) fn validate_graph(
    s: &PageWalStore,
    enabled: bool,
    endpoints_enabled: bool,
    edge_ids_enabled: bool,
    sup: Option<&crate::supportive::header::Supportive>,
) -> Result<()> {
    endpoints::validate_endpoint_sets(s, endpoints_enabled)?;
    if sup.is_none() {
        validate_edge_id_allocator(s, enabled, edge_ids_enabled)?;
    }
    if let Some(sup) = sup {
        // A Register file's graph header and names are entries; the Register
        // verifier owns their damage. What is checked here is agreement.
        let h = crate::collections::register_catalog::read_graph(s, sup)?;
        if enabled != h.is_some() {
            return Err(corrupt("graph census and GRPH disagree"));
        }
        return Ok(());
    }
    if !enabled {
        // A single prefix probe per family is bounded. Edge rows are
        // authoritative data and must never become invisible merely because
        // the required logical feature bit is absent.
        for tag in [
            GRAPH_HEADER,
            NAME_DESCRIPTOR,
            NAME_LOOKUP,
            PRIMARY_EDGE,
            REVERSE_EDGE,
        ] {
            if let Some(row) = s.range(&[tag])?.next() {
                let (key, _) = row?;
                if key.first() == Some(&tag) {
                    return Err(corrupt("graph metadata exists without graph feature"));
                }
            }
        }
        return Ok(());
    }
    let h = read_graph_header(|key| s.get(key).map_err(Error::from))?;
    for row in s.range(&[GRAPH_HEADER])? {
        let (key, value) = row?;
        if key.first() != Some(&GRAPH_HEADER) {
            break;
        }
        if key.len() != 2 || key[1] > 2 {
            return Err(corrupt("graph header replica key"));
        }
        if let Err(e @ Error::Unsupported(_)) = decode_graph_header(&value) {
            return Err(e);
        }
        // A corrupt individual replica is allowed to lose to an intact copy.
    }

    // Inspect every descriptor key so an intact future encoding or orphan
    // cannot be concealed by a good replica. Damaged individual replicas lose.
    let mut descriptor_rows = 0usize;
    for row in s.range(&[NAME_DESCRIPTOR])? {
        let (key, value) = row?;
        if key.first() != Some(&NAME_DESCRIPTOR) {
            break;
        }
        descriptor_rows += 1;
        if descriptor_rows > (MAX_NAMES as usize) * 2 * 3 {
            return Err(corrupt("graph descriptor bound exceeded"));
        }
        if key.len() < 4 || key[1] > 1 || key[2] > 2 {
            return Err(corrupt("graph descriptor key"));
        }
        let mut at = 3;
        let id = read_ordered(&key, &mut at)?;
        if at != key.len() || id == 0 {
            return Err(corrupt("graph descriptor key identity"));
        }
        match decode_name(&value) {
            Ok(n) => {
                if n.kind != key[1] || n.id != id {
                    return Err(corrupt("graph descriptor key/value identity"));
                }
                let expected = ordered(id);
                if s.get(&name_lookup_key(n.kind, &n.name))? != Some(expected) {
                    return Err(corrupt("orphan graph name descriptor"));
                }
            }
            Err(e @ Error::Unsupported(_)) => return Err(e),
            Err(_) => {}
        }
    }

    let mut counts = [0u32; 2];
    for row in s.range(&[NAME_LOOKUP])? {
        let (key, value) = row?;
        if key.first() != Some(&NAME_LOOKUP) {
            break;
        }
        if key.len() < 3 || key[1] > 1 || key.len() - 2 > MAX_NAME_BYTES {
            return Err(corrupt("graph name lookup key"));
        }
        let name = std::str::from_utf8(&key[2..]).map_err(corrupt)?;
        if name.is_empty() {
            return Err(corrupt("empty graph name lookup"));
        }
        let mut at = 0;
        let id = read_ordered(&value, &mut at)?;
        let next = if key[1] == 0 {
            h.next_type
        } else {
            h.next_context
        };
        if at != value.len() || id == 0 || id >= next {
            return Err(corrupt("graph name lookup identity"));
        }
        let n = read_name(|k| s.get(k).map_err(Error::from), key[1], id)?;
        if n.name != name {
            return Err(corrupt("graph name lookup mismatch"));
        }
        counts[key[1] as usize] = counts[key[1] as usize]
            .checked_add(1)
            .ok_or_else(|| corrupt("graph name count overflow"))?;
        if counts[key[1] as usize] > MAX_NAMES {
            return Err(corrupt("graph name count exceeds product limit"));
        }
    }
    if counts != [h.type_count, h.context_count] {
        return Err(corrupt("graph name count mismatch"));
    }
    Ok(())
}

impl Database {
    pub(crate) fn graph_header(&self) -> Result<GraphHeader> {
        let enabled = self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0);
        if !enabled {
            return Err(invalid("graph feature is not enabled"));
        }
        if let Some(cached) = self.graph_header_cache.get() {
            return Ok(cached);
        }
        let h = match &self.supportive {
            Some(sup) => crate::collections::register_catalog::read_graph(self.store()?, sup)?
                .ok_or_else(|| corrupt("graph feature without GRPH"))?,
            None => read_graph_header(|key| self.store()?.get(key).map_err(Error::from))?,
        };
        self.graph_header_cache.set(Some(h));
        Ok(h)
    }

    fn save_graph_header(&mut self, h: GraphHeader) -> Result<()> {
        if let Some(mut sup) = self.supportive.take() {
            let old = self.graph_header_cache.get();
            self.graph_header_cache.set(None);
            let r = self.writer().and_then(|w| {
                let old = match old {
                    Some(o) => Some(o),
                    None => crate::collections::register_catalog::read_graph(w, &sup)?,
                };
                crate::collections::register_catalog::write_graph(w, &mut sup, old, h)
            });
            self.supportive = Some(sup);
            r?;
            self.graph_header_cache.set(Some(h));
            return Ok(());
        }
        let bytes = encode_graph_header(h)?;
        // Publish the cache only once all three replicas are on their way, so
        // a failed write leaves the cache empty rather than ahead of the file.
        self.graph_header_cache.set(None);
        for copy in 0..3 {
            self.writer()?.put(&graph_header_key(copy), &bytes)?;
        }
        self.graph_header_cache.set(Some(h));
        Ok(())
    }

    fn save_graph_name(&mut self, n: &GraphName) -> Result<()> {
        if let Some(mut sup) = self.supportive.take() {
            let r = self.writer().and_then(|w| {
                crate::collections::register_catalog::put_graph_name(w, &mut sup, n.kind, n.id, &n.name)
            });
            self.supportive = Some(sup);
            return r;
        }
        let bytes = encode_name(n)?;
        for copy in 0..3 {
            self.writer()?
                .put(&name_descriptor_key(n.kind, copy, n.id), &bytes)?;
        }
        self.writer()?
            .put(&name_lookup_key(n.kind, &n.name), &ordered(n.id))?;
        Ok(())
    }

    fn lookup_graph_name(&self, kind: u8, name: &str) -> Result<Option<u64>> {
        if let Some(sup) = &self.supportive {
            return crate::collections::register_catalog::graph_name_id(self.store()?, sup, kind, name);
        }
        let Some(value) = self.store()?.get(&name_lookup_key(kind, name))? else {
            return Ok(None);
        };
        let mut at = 0;
        let id = read_ordered(&value, &mut at)?;
        if at != value.len() || id == 0 {
            return Err(corrupt("graph name lookup value"));
        }
        let descriptor = read_name(|key| self.store()?.get(key).map_err(Error::from), kind, id)?;
        if descriptor.name != name {
            return Err(corrupt("graph name lookup descriptor mismatch"));
        }
        Ok(Some(id))
    }

    /// Explicit logical-format opt-in. The caller publishes it with `commit`.
    pub fn enable_graph(&mut self) -> Result<()> {
        self.user_write()?;
        if self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0)
        {
            self.graph_header()?;
            return Ok(());
        }
        let mut header = self.index_header.unwrap_or(IndexHeader {
            features: 1 | GRAPH_FEATURE,
            next: 1,
            count: 0,
        });
        header.features |= GRAPH_FEATURE;
        let graph = GraphHeader {
            next_type: 1,
            next_context: 1,
            type_count: 0,
            context_count: 0,
        };
        let (next_collection, next_layout) = self.header()?;
        let result = (|| {
            self.index_header = Some(header);
            self.write_header(next_collection, next_layout)?;
            self.save_graph_header(graph)
        })();
        self.finish(result)
    }

    pub fn edge_type(&self, name: &str) -> Result<Option<EdgeTypeId>> {
        validate_name_input(name, "edge type name")?;
        self.graph_header()?;
        Ok(self.lookup_graph_name(0, name)?.map(EdgeTypeId))
    }

    /// Every INTERNED edge-type name and every interned graph-context name,
    /// each with the identity it was interned under, in key order.
    ///
    /// The graph's name dictionary read as CATALOG DATA, which is what
    /// `docs/core/GRAPH_CONTRACT.md` 2.5 and 3.4 call it: an edge type is
    /// interned on first use and a context descriptor is catalog data, so a
    /// tool can list both without reading one edge. This walks the
    /// name-lookup keyspace (tag `0x12`, `kind || name` -> identity), which
    /// holds ONE entry per name; the dictionary is capped at 4,096 names per
    /// kind by `create_graph_name`, so the walk is at most 8,192 entries and
    /// reads no edge, no row and no index.
    ///
    /// The base graph is not in the dictionary: it has no descriptor and no
    /// name (`graph_context_name` spells it `(base graph)`), so a caller that
    /// wants it in a listing adds it.
    ///
    /// Empty, not an error, on a database whose graph feature was never
    /// enabled: no dictionary is not a corrupt dictionary.
    pub fn graph_names(
        &self,
    ) -> Result<(Vec<(EdgeTypeId, String)>, Vec<(GraphContextId, String)>)> {
        let mut types = Vec::new();
        let mut contexts = Vec::new();
        if !self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0)
        {
            return Ok((types, contexts));
        }
        self.graph_header()?;
        if let Some(sup) = &self.supportive {
            let store = self.store()?;
            for (name, id) in crate::collections::register_catalog::graph_name_list(store, sup, 0)? {
                types.push((EdgeTypeId(id), name));
            }
            for (name, id) in crate::collections::register_catalog::graph_name_list(store, sup, 1)? {
                contexts.push((GraphContextId(id), name));
            }
            return Ok((types, contexts));
        }
        for row in self.store()?.range(&[NAME_LOOKUP])? {
            let (key, value) = row?;
            if key.first() != Some(&NAME_LOOKUP) {
                break;
            }
            if key.len() < 2 || key[1] > 1 {
                return Err(corrupt("graph name lookup key"));
            }
            if types.len() + contexts.len() > (MAX_NAMES as usize) * 2 {
                return Err(corrupt("graph name dictionary bound exceeded"));
            }
            let name = std::str::from_utf8(&key[2..])
                .map_err(|_| corrupt("graph name lookup encoding"))?
                .to_owned();
            let mut at = 0;
            let id = read_ordered(&value, &mut at)?;
            if at != value.len() || id == 0 {
                return Err(corrupt("graph name lookup value"));
            }
            if key[1] == 0 {
                types.push((EdgeTypeId(id), name));
            } else {
                contexts.push((GraphContextId(id), name));
            }
        }
        Ok((types, contexts))
    }

    /// The catalog's EDGE-TYPE ROWS: which collections an edge type connects,
    /// in which context, derived from written edges.
    ///
    /// `docs/core/GRAPH_CONTRACT.md` 2.5 states this exactly -- "the catalog's
    /// edge-type rows (which collections an edge type connects) are derived
    /// from written edges, so a tool sees the graph shape without any
    /// declaration" -- and this is the reader for it. One entry per DISTINCT
    /// `(source collection, context, edge type, destination collection)`,
    /// ascending, without duplicates.
    ///
    /// **The bound, stated.** The primary edge keyspace is
    /// `tag | source | context | type | destination`, and the source's
    /// SEQUENCE sits between the collection and the context, so a triple is
    /// not a key prefix: the walk pays one descent per distinct
    /// `(source entity, context, type, destination collection)` and seeks
    /// past each such run rather than stepping through its edges. It is
    /// therefore proportional to the SOURCE ENTITIES that have edges, never
    /// to the edges, and it is capped: at most `cap` descents, after which it
    /// returns what it found with `true` for truncated. A caller that prints
    /// these rows says so -- `docs/lang/QL_CONTRACT.md` §2 labels the
    /// `SHOW EDGES` counts a scan for the same reason.
    pub fn edge_shape(&self, cap: usize) -> Result<(Vec<EdgeShape>, bool)> {
        if !self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0)
        {
            return Ok((Vec::new(), false));
        }
        let h = self.graph_header()?;
        let mut found: BTreeSet<EdgeShape> = BTreeSet::new();
        let mut truncated = false;
        let mut from = vec![PRIMARY_EDGE];
        let mut seeks = 0usize;
        loop {
            if seeks >= cap {
                truncated = true;
                break;
            }
            seeks += 1;
            let Some(row) = self.store()?.range(&from)?.next() else {
                break;
            };
            let (key, _) = row?;
            if key.first() != Some(&PRIMARY_EDGE) {
                break;
            }
            let (edge, _) = parse_edge(&key, PRIMARY_EDGE)?;
            self.validate_stored_edge_ids(h, edge)?;
            found.insert(EdgeShape {
                edge_type: edge.edge_type,
                context: edge.context,
                from: edge.source.collection,
                to: edge.destination.collection,
            });
            // Past every remaining edge of this (source, context, type) whose
            // destination is in the SAME collection: the destination's
            // collection is the first component of its identity, so
            // `destination collection + 1` is the next distinct triple.
            let Some(next) = u64::from(edge.destination.collection.0).checked_add(1) else {
                break;
            };
            from = edge_prefix(
                PRIMARY_EDGE,
                edge.source,
                Some(edge.context),
                Some(edge.edge_type),
            );
            from.extend(ordered(next));
        }
        Ok((found.into_iter().collect(), truncated))
    }

    /// Every entity of `collection` that has AT LEAST ONE edge of
    /// `edge_type` in `context`, ascending, without duplicates.
    ///
    /// The semi-join half of `docs/lang/QL_CONTRACT.md` §3's `EXISTS`: an edge
    /// table's `source` column is this walk with `Direction::Outgoing` and
    /// its `destination` column the same walk over the reverse mirror, so
    /// `EXISTS (SELECT 1 FROM related WHERE related.source = t._key)` is one
    /// keyspace walk rather than a probe per outer row.
    ///
    /// The keyspace is `tag || near entity || context || type || far
    /// entity`, and the near entity's COLLECTION is the first thing in it, so
    /// the walk starts at this collection's own stretch and stops the moment
    /// it leaves it -- it no longer steps over every other collection's
    /// edges. Inside that stretch the near entities come out in order and the
    /// walk SEEKS past the near entity as soon as one of its edges matches:
    /// an entity with a thousand edges costs one step, not a thousand. The
    /// doc comment claimed that skip before the walk made it.
    ///
    /// Three bounds, each named where it is hit. `max_edges` bounds the steps
    /// and refuses with `GraphEdges`; `max_ids` bounds the SET this returns --
    /// it is held in memory for the length of the caller's statement -- and
    /// refuses with `GraphVisited`; and `meter` is the caller's own budget and
    /// cancellation, so the walk is charged `GraphEdges` per step and polls
    /// the caller's cancel between them. Before this the loop had none of the
    /// three: `EXISTS` over a 60-million-edge graph allocated its ids with
    /// Ctrl-C inert.
    pub fn edge_endpoints<C: FnMut() -> bool>(
        &self,
        collection: CollectionId,
        context: GraphContextId,
        edge_type: EdgeTypeId,
        direction: Direction,
        max_edges: usize,
        max_ids: usize,
        budget: crate::query::QueryBudget,
        cancelled: C,
    ) -> Result<Vec<EntityId>> {
        let mut cancelled = cancelled;
        // The ENDPOINT SET answers this question directly when the file
        // carries one: one posting per distinct entity in one contiguous
        // range, no seek over an edge (`index/graph/endpoints.rs`). A file
        // that does not carry one takes the walk below, which is what every
        // release before the set took.
        if self.endpoint_sets_present() {
            self.graph_header()?;
            return self.endpoints_from_set(
                collection,
                context,
                edge_type,
                direction,
                max_ids,
                budget,
                &mut cancelled,
            );
        }
        let meter = &mut crate::query::WorkMeter::new(budget, &mut cancelled);
        let tag = match direction {
            Direction::Outgoing => PRIMARY_EDGE,
            Direction::Incoming => REVERSE_EDGE,
            Direction::Both => {
                return Err(invalid(
                    "an edge-endpoint set names one direction: a column of an edge table is either its source or its destination",
                ))
            }
        };
        self.graph_header()?;
        // `tag || ordered(collection)`: the ordered integer encoding is
        // length-tagged, so one collection's stretch cannot be the prefix of
        // another's and this is the whole of the near end's collection.
        let mut prefix = vec![tag];
        prefix.extend(crate::collections::ordered(u64::from(collection.0)));
        let mut out: Vec<EntityId> = Vec::new();
        let mut walk = self.store()?.range(&prefix)?;
        let mut edges = 0usize;
        // Set once an entity has matched: the next peek resumes at the first
        // key past every edge of that entity instead of stepping through them.
        let mut resume_at: Option<Vec<u8>> = None;
        loop {
            meter.check_cancelled()?;
            let peeked = match &resume_at {
                Some(target) => walk.peek_at_or_after(target)?,
                None => walk.peek_ref()?,
            };
            let Some((key, _)) = peeked else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            resume_at = None;
            edges += 1;
            if edges > max_edges {
                return Err(Error::BudgetExceeded {
                    resource: crate::query::WorkResource::GraphEdges,
                    limit: max_edges as u64,
                    attempted: edges as u64,
                });
            }
            let mut at = 1;
            let near = read_entity(key, &mut at)?;
            let near_end = at;
            let entry_context = GraphContextId(read_ordered(key, &mut at)?);
            let entry_type = EdgeTypeId(read_ordered(key, &mut at)?);
            if entry_context != context || entry_type != edge_type {
                walk.step();
                meter.charge(crate::query::WorkResource::GraphEdges, 1)?;
                continue;
            }
            // One edge of this entity is enough. Everything else filed under
            // it answers the same question, so the walk steps over the whole
            // run in one seek.
            let skip = key_after_prefix(&key[..near_end]);
            meter.charge(crate::query::WorkResource::GraphEdges, 1)?;
            meter.charge(crate::query::WorkResource::GraphVisited, 1)?;
            out.push(near);
            if out.len() > max_ids {
                return Err(Error::BudgetExceeded {
                    resource: crate::query::WorkResource::GraphVisited,
                    limit: max_ids as u64,
                    attempted: out.len() as u64,
                });
            }
            match skip {
                Some(target) => resume_at = Some(target),
                // Every byte was 0xFF: there is no key past this entity.
                None => break,
            }
        }
        // The walk hands them over in key order, which is ascending sequence
        // inside one collection, and the seek makes each entity appear once.
        // Kept as insurance, not as the source of the order.
        out.sort_unstable_by_key(|id| id.sequence);
        out.dedup();
        Ok(out)
    }

    pub fn graph_context(&self, name: &str) -> Result<Option<GraphContextId>> {
        if name.is_empty() {
            self.graph_header()?;
            return Ok(Some(GraphContextId::BASE));
        }
        validate_name_input(name, "graph context name")?;
        self.graph_header()?;
        Ok(self.lookup_graph_name(1, name)?.map(GraphContextId))
    }

    fn create_graph_name(&mut self, kind: u8, name: &str) -> Result<u64> {
        // Reached only through `create_edge_type` / `create_graph_context`,
        // both public: naming an edge type is the caller's decision.
        self.user_write()?;
        validate_name_input(
            name,
            if kind == 0 {
                "edge type name"
            } else {
                "graph context name"
            },
        )?;
        let mut h = self.graph_header()?;
        if self.lookup_graph_name(kind, name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let (next, count) = if kind == 0 {
            (&mut h.next_type, &mut h.type_count)
        } else {
            (&mut h.next_context, &mut h.context_count)
        };
        if *count >= MAX_NAMES {
            return Err(invalid("graph name dictionary limit is 4096"));
        }
        let id = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| invalid("graph name identities exhausted"))?;
        *count += 1;
        let n = GraphName {
            kind,
            id,
            name: name.into(),
        };
        let result = (|| {
            self.save_graph_name(&n)?;
            self.save_graph_header(h)?;
            Ok(id)
        })();
        self.finish(result)
    }

    pub fn create_edge_type(&mut self, name: &str) -> Result<EdgeTypeId> {
        self.create_graph_name(0, name).map(EdgeTypeId)
    }

    pub fn create_graph_context(&mut self, name: &str) -> Result<GraphContextId> {
        self.create_graph_name(1, name).map(GraphContextId)
    }

    fn validate_edge_ids(&self, h: GraphHeader, key: EdgeKey) -> Result<()> {
        if key.edge_type.0 == 0 || key.edge_type.0 >= h.next_type {
            return Err(invalid("unknown edge type identity"));
        }
        if key.context.0 >= h.next_context {
            return Err(invalid("unknown graph context identity"));
        }
        Ok(())
    }

    fn validate_stored_edge_ids(&self, h: GraphHeader, key: EdgeKey) -> Result<()> {
        if key.edge_type.0 == 0 || key.edge_type.0 >= h.next_type || key.context.0 >= h.next_context
        {
            return Err(corrupt("stored edge has unknown type/context identity"));
        }
        Ok(())
    }

    fn validate_endpoints(&self, source: EntityId, destination: EntityId) -> Result<()> {
        self.validate_endpoint(source)?;
        self.validate_endpoint(destination)
    }

    /// One endpoint of one edge. `link_many` calls it once per DISTINCT
    /// entity of a batch, which is the same check with the repeats removed.
    fn validate_endpoint(&self, id: EntityId) -> Result<()> {
        if id.collection.0 == 0 || id.sequence == 0 {
            return Err(Error::NotFound("graph endpoint"));
        }
        // A collection under `begin_drop_collection` accepts no new edge.
        // Without this a RESTRICT drop could be admitted on an empty probe and
        // then have an edge written into it, and the drop would remove that
        // edge without anyone asking for CASCADE. The handle field is `None`
        // on every database with no drop in flight, so this is one comparison
        // per endpoint, not a descriptor read.
        if self.dropping == Some(id.collection) {
            return Err(invalid(
                "graph endpoint is in a collection that is DROPPING; no edge may be written onto it",
            ));
        }
        // The existence check stays -- an edge to a row that is not there is
        // the dangling reference this guard exists to refuse. It is the
        // DESCENT that goes away, and only for a row this handle wrote and has
        // not deleted, where the answer is already known.
        if self.endpoint_known_live(id) {
            return Ok(());
        }
        if self.store()?.get(&row_key(id))?.is_none() {
            return Err(Error::NotFound("graph endpoint"));
        }
        Ok(())
    }

    /// Returns whether the pair exists. See
    /// `preflight_unless_provably_absent` for why the caller wants to know.
    fn preflight_edge_pair(&self, key: EdgeKey) -> Result<bool> {
        let mut scratch = Vec::with_capacity(EDGE_KEY_BYTES);
        edge_key_into(&mut scratch, PRIMARY_EDGE, key);
        let primary = self.store()?.get(&scratch)?;
        scratch.clear();
        edge_key_into(&mut scratch, REVERSE_EDGE, key);
        let reverse = self.store()?.get(&scratch)?;
        match (primary, reverse) {
            (Some(value), Some(marker)) => {
                decode_properties(&value)?;
                if !marker.is_empty() {
                    return Err(corrupt("nonempty reverse edge marker"));
                }
                Ok(true)
            }
            (None, None) => Ok(false),
            _ => Err(corrupt("graph primary/reverse mismatch")),
        }
    }

    /// Both halves of an edge, written immediately.
    ///
    /// DEFERRED, with the reason recorded so it is not re-proposed blind:
    /// buffering the derived halves of a batch and flushing each tag as one
    /// ascending run at commit. Two findings stop it.
    ///
    /// * It would not arm the append fast path. `fast_path_leaf`
    ///   (`core/kernel/src/btree.rs:1928`) accepts a hinted leaf only when
    ///   `next_leaf() == 0` (check 3, `:1941`) -- rightmost in the WHOLE tree,
    ///   not in its tag -- and a collection's edge keyspace is one tree
    ///   (`kernel::Store` in `core/kernel/src/store.rs`, one `tree_id` per
    ///   collection). Only the highest tag present can ever satisfy that, so
    ///   sorting `0x71`/`0x72` into runs still pays a full descent per write.
    ///   Relaxing that check needs a right-hand bound the hint can trust
    ///   across a neighbour's growth; getting it wrong appends keys that a
    ///   scan finds and a `get` does not.
    /// * Deferred entries would be invisible to same-transaction readers.
    ///   `preflight_edge_pair`, `delete_edge`, `neighbors`, `bfs` and
    ///   `remove_node` (here) and the traversal in `src/query/drivers.rs` all
    ///   read the edge keyspace through `&self`, several by range scan, so
    ///   they cannot
    ///   flush a buffer and cannot cheaply merge one. Correct writes that a
    ///   read in the same transaction cannot see are not a trade this engine
    ///   makes.
    ///
    /// So the edge family stays immediate, and the read side is what got
    /// cheaper instead -- see `preflight_unless_provably_absent`.
    fn write_edge_pair(&mut self, key: EdgeKey, properties: &[u8]) -> Result<()> {
        // One buffer for both halves. They are the same integers in a
        // different order and neither outlives the put that reads it.
        let mut scratch = Vec::with_capacity(EDGE_KEY_BYTES);
        edge_key_into(&mut scratch, PRIMARY_EDGE, key);
        self.writer()?.put(&scratch, properties)?;
        scratch.clear();
        edge_key_into(&mut scratch, REVERSE_EDGE, key);
        self.writer()?.put(&scratch, &[])?;
        Ok(())
    }

    /// The forward/reverse probe, skipped when the pair provably cannot exist.
    ///
    /// The probe's whole product is a corruption report about a pair this call
    /// is on its way to overwrite anyway. When one endpoint is an identity
    /// this handle allocated (D13: never reused, counter rides the commit) and
    /// no edge with this key has been written on it since, no such pair is on
    /// disk to report on, and the two descents buy nothing.
    ///
    /// SACRIFICE (Law 4): a corrupt forward/reverse pair on such an endpoint
    /// is no longer reported by THIS call -- it is overwritten by a correct
    /// pair instead. Nothing is believed from the damaged bytes, no other
    /// record depends on them, and `src/collections/verification.rs`'s
    /// `verify_actual` still finds a mismatch it can reach. What is lost is
    /// an early warning, not a repair.
    /// Returns whether the pair is ALREADY on disk, which is what tells the
    /// endpoint sets whether this edge is NEW: a new edge files its two ends,
    /// a repeated one files nothing. The answer is free -- it is the probe
    /// this method already ran -- and a provably absent pair is provably new.
    fn preflight_unless_provably_absent(&self, key: EdgeKey) -> Result<bool> {
        if self.edge_provably_absent(key) {
            return Ok(false);
        }
        self.preflight_edge_pair(key)
    }

    /// Identity-based ingestion path. Resolving names once avoids replicated
    /// descriptor reads for every relationship in a batch.
    pub fn put_edge(
        &mut self,
        context: GraphContextId,
        source: EntityId,
        edge_type: EdgeTypeId,
        destination: EntityId,
        properties: &Value,
    ) -> Result<EdgeKey> {
        self.refuse_bound_edge_type(edge_type)?;
        self.put_edge_inner(context, source, edge_type, destination, properties)
    }

    /// `put_edge` for an edge table's own write, which has checked the
    /// edge's types and key (`edge_table.rs`).
    pub(crate) fn put_edge_typed(&mut self, key: EdgeKey, properties: &Value) -> Result<EdgeKey> {
        self.put_edge_inner(key.context, key.source, key.edge_type, key.destination, properties)
    }

    fn put_edge_inner(
        &mut self,
        context: GraphContextId,
        source: EntityId,
        edge_type: EdgeTypeId,
        destination: EntityId,
        properties: &Value,
    ) -> Result<EdgeKey> {
        self.ready_write()?;
        let h = self.graph_header()?;
        let key = EdgeKey {
            source,
            context,
            edge_type,
            destination,
        };
        self.validate_edge_ids(h, key)?;
        self.validate_endpoints(source, destination)?;
        let bytes = encode_properties(properties)?;
        if self
            .limits
            .is_some_and(|l| bytes.len() + 64 > l.record_bytes as usize)
        {
            return Err(invalid("encoded edge exceeds configured record limit"));
        }
        let existed = self.preflight_unless_provably_absent(key)?;
        let result = (|| {
            // Inside the closure, because it can WRITE the header that turns
            // the feature on, and a failed write must poison this handle.
            // Before `write_edge_pair`, because it probes the edge keyspace
            // for emptiness: see `endpoints.rs`.
            let maintain = self.endpoint_maintenance()?;
            self.write_edge_pair(key, &bytes)?;
            if maintain && !existed {
                self.insert_endpoint_keys(key)?;
            }
            Ok(key)
        })();
        self.finish(result)
    }

    /// Many edges of ONE context and ONE type, written under one validation
    /// pass and in key order.
    ///
    /// The same work `put_edge` does, hoisted to what it actually depends on.
    /// `ready_write`, the graph header read and the type/context identity
    /// check belong to the BATCH, not to the edge. Endpoint existence is a
    /// property of an ENTITY, so it is checked once per DISTINCT entity in the
    /// batch: a source with three edges paid for three identical descents
    /// before, and a destination that is also a source paid twice more.
    ///
    /// The forward/reverse probe stays PER EDGE and stays BEFORE every write,
    /// so no probe in this batch can read a primary that this same batch wrote
    /// without its reverse; its skip rule is `put_edge`'s, unchanged.
    ///
    /// The writes are then issued in key order, one keyspace at a time: every
    /// `0x71` posting ascending, then every `0x72` posting ascending. The
    /// descent per key is unchanged -- `fast_path_leaf` still wants the
    /// rightmost leaf of the WHOLE tree, for the reason `write_edge_pair`
    /// records -- but a sorted run walks each leaf and each internal node once
    /// and in order, so the pages one descent needs are the pages the previous
    /// descent just left in the pool. One `put_edge` per edge alternates
    /// between two distant stretches of the same tree on every single edge,
    /// and on a buffer pool smaller than the tree each stretch evicts the
    /// other's path.
    ///
    /// Duplicates inside one batch are last-wins, the same rule two
    /// consecutive `put_edge` calls of one quadruple follow: the sort is
    /// stable, so the later entry is written last.
    ///
    /// L3 (`docs/core/FOUNDATION_TEST_STANDARD.md`): nothing commits here.
    /// The call returns only once both directions of every edge are in the
    /// transaction, and any failure poisons the handle, so no commit can ever
    /// publish a primary posting without its reverse marker.
    ///
    /// Returns the edge keys, in the caller's order.
    pub fn link_many(
        &mut self,
        context: GraphContextId,
        edge_type: EdgeTypeId,
        edges: &[NewEdge],
    ) -> Result<Vec<EdgeKey>> {
        self.ready_write()?;
        self.refuse_bound_edge_type(edge_type)?;
        if edges.is_empty() {
            return Ok(Vec::new());
        }
        let h = self.graph_header()?;
        let result = (|| {
            // Once for the batch, and before any write of it, for the reason
            // `put_edge` calls it where it does.
            let maintain = self.endpoint_maintenance()?;
            // One identity check for the whole batch: the context and the type
            // are the batch's, so `validate_edge_ids` has one answer for it.
            self.validate_edge_ids(
                h,
                EdgeKey {
                    source: edges[0].source,
                    context,
                    edge_type,
                    destination: edges[0].destination,
                },
            )?;
            // One existence check per DISTINCT entity. The set is bounded by
            // the batch the caller handed in, not by the graph.
            let mut endpoints: BTreeSet<(u32, u64)> = BTreeSet::new();
            for edge in edges {
                endpoints.insert((edge.source.collection.0, edge.source.sequence));
                endpoints.insert((edge.destination.collection.0, edge.destination.sequence));
            }
            for (collection, sequence) in &endpoints {
                self.validate_endpoint(EntityId {
                    collection: CollectionId(*collection),
                    sequence: *sequence,
                })?;
            }
            let mut keys = Vec::with_capacity(edges.len());
            let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(edges.len());
            for edge in edges {
                let bytes = encode_properties(&edge.properties)?;
                if self
                    .limits
                    .is_some_and(|l| bytes.len() + 64 > l.record_bytes as usize)
                {
                    return Err(invalid("encoded edge exceeds configured record limit"));
                }
                keys.push(EdgeKey {
                    source: edge.source,
                    context,
                    edge_type,
                    destination: edge.destination,
                });
                bodies.push(bytes);
            }
            // Ascending by primary key. `sort_by_key` is stable, which is what
            // makes a repeated quadruple inside one batch last-wins.
            let mut order: Vec<usize> = (0..keys.len()).collect();
            order.sort_by_key(|i| edge_key(PRIMARY_EDGE, keys[*i]));
            // EVERY probe before ANY write, in key order: no probe in this
            // batch can read a primary this same batch wrote without its
            // reverse, and the probes walk the keyspace once instead of
            // jumping between two stretches of it per edge.
            //
            // The probe's answer is also what says which edges are NEW, so
            // the ENDPOINT SET keys of the batch cost no read of their own.
            // They are DEDUPLICATED across the batch -- a source with three
            // new edges files one key, not three -- and written last, in
            // ascending key order.
            let mut endpoint_keys: BTreeSet<Vec<u8>> = BTreeSet::new();
            for i in &order {
                let existed = self.preflight_unless_provably_absent(keys[*i])?;
                if maintain && !existed {
                    let key = keys[*i];
                    for (dir, entity) in endpoints::ends(key) {
                        endpoint_keys.insert(endpoints::endpoint_key(
                            key.context,
                            key.edge_type,
                            dir,
                            entity,
                        ));
                    }
                }
            }
            let mut scratch = Vec::with_capacity(EDGE_KEY_BYTES);
            for i in &order {
                scratch.clear();
                edge_key_into(&mut scratch, PRIMARY_EDGE, keys[*i]);
                self.writer()?.put(&scratch, &bodies[*i])?;
            }
            order.sort_by_key(|i| edge_key(REVERSE_EDGE, keys[*i]));
            for i in &order {
                scratch.clear();
                edge_key_into(&mut scratch, REVERSE_EDGE, keys[*i]);
                self.writer()?.put(&scratch, &[])?;
            }
            for key in endpoint_keys {
                if self.endpoint_written.contains(&key) {
                    continue;
                }
                self.writer()?.put(&key, &[])?;
                if self.endpoint_written.len() < crate::collections::ENDPOINT_MEMO {
                    self.endpoint_written.insert(key);
                }
            }
            Ok(keys)
        })();
        self.finish(result)
    }

    /// DIAGNOSTIC TWIN of `put_edge`, used only by `bench/src/bin/g2_budget.rs`.
    ///
    /// It performs the same sequence of steps in the same order and times each
    /// one, in nanoseconds and in buffer-pool page accesses. It is a separate
    /// body so the shipping path carries no timer and no branch: the stages it
    /// names are the stages `put_edge` above runs, and a change to one is a
    /// change to both.
    #[doc(hidden)]
    pub fn put_edge_measured(
        &mut self,
        context: GraphContextId,
        source: EntityId,
        edge_type: EdgeTypeId,
        destination: EntityId,
        properties: &Value,
        budget: &mut EdgeBudget,
    ) -> Result<EdgeKey> {
        use std::time::Instant;
        let mut at = Instant::now();
        let mut pool = self.store()?.store().pool_accesses();
        macro_rules! lap {
            ($ns:ident, $pa:ident) => {{
                let now = Instant::now();
                budget.$ns += now.duration_since(at).as_nanos() as u64;
                at = now;
                let p = self.store()?.store().pool_accesses();
                budget.$pa += p - pool;
                pool = p;
            }};
        }
        let key = EdgeKey {
            source,
            context,
            edge_type,
            destination,
        };
        // Which fast paths this edge is ELIGIBLE for.
        budget.fast_endpoints += u64::from(self.endpoint_known_live(source))
            + u64::from(self.endpoint_known_live(destination));
        budget.fast_preflight += u64::from(self.edge_provably_absent(key));
        at = Instant::now();
        pool = self.store()?.store().pool_accesses();
        self.ready_write()?;
        let h = self.graph_header()?;
        self.validate_edge_ids(h, key)?;
        lap!(header_ns, header_pages);
        self.validate_endpoints(source, destination)?;
        lap!(endpoints_ns, endpoint_pages);
        let bytes = encode_properties(properties)?;
        if self
            .limits
            .is_some_and(|l| bytes.len() + 64 > l.record_bytes as usize)
        {
            return Err(invalid("encoded edge exceeds configured record limit"));
        }
        lap!(encode_ns, encode_pages);
        let started = self.endpoint_maintenance();
        let maintain = self.finish(started)?;
        let existed = self.preflight_unless_provably_absent(key)?;
        lap!(preflight_ns, preflight_pages);
        let primary = edge_key(PRIMARY_EDGE, key);
        let reverse = edge_key(REVERSE_EDGE, key);
        lap!(keybuild_ns, keybuild_pages);
        self.writer()?.put(&primary, &bytes)?;
        lap!(put_primary_ns, put_primary_pages);
        self.writer()?.put(&reverse, &[])?;
        lap!(put_reverse_ns, put_reverse_pages);
        if maintain && !existed {
            self.insert_endpoint_keys(key)?;
        }
        lap!(endpoint_keys_ns, endpoint_keys_pages);
        let out = self.finish(Ok(key));
        lap!(finish_ns, finish_pages);
        budget.edges += 1;
        out
    }

    /// String convenience call. Unknown exact names are allocated in this
    /// transaction; an empty context selects `GraphContextId::BASE`.
    pub fn link(
        &mut self,
        source: EntityId,
        edge_type: &str,
        destination: EntityId,
        context: &str,
        properties: &Value,
    ) -> Result<EdgeKey> {
        self.user_write()?;
        validate_name_input(edge_type, "edge type name")?;
        if !context.is_empty() {
            validate_name_input(context, "graph context name")?;
        }
        self.validate_endpoints(source, destination)?;
        let bytes = encode_properties(properties)?;
        if self
            .limits
            .is_some_and(|l| bytes.len() + 64 > l.record_bytes as usize)
        {
            return Err(invalid("encoded edge exceeds configured record limit"));
        }
        let mut h = self.graph_header()?;
        let mut names = Vec::new();
        let type_id = match self.lookup_graph_name(0, edge_type)? {
            Some(id) => id,
            None => {
                if h.type_count >= MAX_NAMES {
                    return Err(invalid("graph name dictionary limit is 4096"));
                }
                let id = h.next_type;
                h.next_type = h
                    .next_type
                    .checked_add(1)
                    .ok_or_else(|| invalid("graph type identities exhausted"))?;
                h.type_count += 1;
                names.push(GraphName {
                    kind: 0,
                    id,
                    name: edge_type.into(),
                });
                id
            }
        };
        let context_id = if context.is_empty() {
            0
        } else {
            match self.lookup_graph_name(1, context)? {
                Some(id) => id,
                None => {
                    if h.context_count >= MAX_NAMES {
                        return Err(invalid("graph name dictionary limit is 4096"));
                    }
                    let id = h.next_context;
                    h.next_context = h
                        .next_context
                        .checked_add(1)
                        .ok_or_else(|| invalid("graph context identities exhausted"))?;
                    h.context_count += 1;
                    names.push(GraphName {
                        kind: 1,
                        id,
                        name: context.into(),
                    });
                    id
                }
            }
        };
        let key = EdgeKey {
            source,
            context: GraphContextId(context_id),
            edge_type: EdgeTypeId(type_id),
            destination,
        };
        self.refuse_bound_edge_type(key.edge_type)?;
        let existed = self.preflight_unless_provably_absent(key)?;
        let result = (|| {
            let maintain = self.endpoint_maintenance()?;
            for name in &names {
                self.save_graph_name(name)?;
            }
            if !names.is_empty() {
                self.save_graph_header(h)?;
            }
            self.write_edge_pair(key, &bytes)?;
            if maintain && !existed {
                self.insert_endpoint_keys(key)?;
            }
            Ok(key)
        })();
        self.finish(result)
    }

    /// Count every edge in the store by WALKING the primary edge keyspace.
    ///
    /// A SCAN, named as one (`docs/dist/OPS_CONTRACT.md` §6.1): E4 keeps no
    /// O(1) edge counter, and the walk is linear in the number of edges. Only
    /// the primary direction is counted, so an edge is counted once and not
    /// twice. It lives here because the keyspace tag is this module's own.
    pub fn scan_count_edges(&self) -> Result<u64> {
        let prefix = [PRIMARY_EDGE];
        let mut seen = 0u64;
        for row in self.store()?.range(&prefix)? {
            let (k, _) = row?;
            if !k.starts_with(&prefix) {
                break;
            }
            seen += 1;
        }
        Ok(seen)
    }
    /// Delete by TUPLE: every edge of `(source, context, type, destination)`
    /// -- the tuple's own implicit edge and every parallel edge
    /// [`Database::create_edge`] put beside it. On a file without parallel
    /// edges that is the one edge it always was. Returns whether anything was
    /// removed. To remove ONE parallel edge, use
    /// [`Database::delete_edge_by_id`].
    ///
    /// The tuple's edges are one contiguous range (the implicit key is a byte
    /// prefix of exactly its own parallel keys), collected before any is
    /// deleted, and more than 65,536 of them is refused rather than walked.
    pub fn delete_edge(&mut self, key: EdgeKey) -> Result<bool> {
        self.user_write()?;
        let h = self.graph_header()?;
        self.validate_edge_ids(h, key)?;
        let ids = self.tuple_edge_ids(key)?;
        if ids.is_empty() {
            return Ok(false);
        }
        for id in &ids {
            self.preflight_edge_id(key, *id)?;
        }
        let maintain = self.endpoint_sets_present();
        let result = (|| {
            for id in &ids {
                if !self.writer()?.delete(&edge_key_id(PRIMARY_EDGE, key, *id))?
                    || !self.writer()?.delete(&edge_key_id(REVERSE_EDGE, key, *id))?
                {
                    return Err(corrupt("edge disappeared during delete"));
                }
            }
            // AFTER the delete, so the probe cannot find the edge it is
            // retiring. One bounded range probe per end; see `endpoints.rs`.
            if maintain {
                self.remove_endpoint_keys_if_last(key)?;
            }
            Ok(true)
        })();
        self.finish(result)
    }

    /// The ids of every edge of one tuple, ascending, with a reverse-only
    /// id (a mirror whose authoritative primary is gone) reported as the
    /// corruption it is.
    fn tuple_edge_ids(&self, key: EdgeKey) -> Result<Vec<u64>> {
        let mut ids = Vec::new();
        let primary = edge_key(PRIMARY_EDGE, key);
        for row in self.store()?.range(&primary)? {
            let (k, _) = row?;
            if !k.starts_with(&primary) {
                break;
            }
            let mut at = primary.len();
            ids.push(read_edge_id(&k, &mut at)?);
            if ids.len() > MAX_TUPLE_EDGES {
                return Err(invalid(
                    "more than 65536 parallel edges share this tuple; delete them by id",
                ));
            }
        }
        let reverse = edge_key(REVERSE_EDGE, key);
        let mut mirrors = 0usize;
        for row in self.store()?.range(&reverse)? {
            let (k, _) = row?;
            if !k.starts_with(&reverse) {
                break;
            }
            let mut at = reverse.len();
            let id = read_edge_id(&k, &mut at)?;
            if ids.binary_search(&id).is_err() {
                return Err(corrupt("reverse edge without authoritative primary"));
            }
            mirrors += 1;
        }
        if mirrors != ids.len() {
            return Err(corrupt("missing/nonempty reverse edge marker"));
        }
        Ok(ids)
    }

    /// Both halves of ONE edge, checked: `Ok(true)` when the pair is intact,
    /// `Ok(false)` when neither half exists, and `Corrupt` for half a pair or
    /// a damaged bag.
    fn preflight_edge_id(&self, key: EdgeKey, id: u64) -> Result<bool> {
        let primary = self.store()?.get(&edge_key_id(PRIMARY_EDGE, key, id))?;
        let reverse = self.store()?.get(&edge_key_id(REVERSE_EDGE, key, id))?;
        let Some(value) = primary else {
            if reverse.is_some() {
                return Err(corrupt("reverse edge without authoritative primary"));
            }
            return Ok(false);
        };
        decode_properties(&value)?;
        if reverse.as_deref() != Some(&[]) {
            return Err(corrupt("missing/nonempty reverse edge marker"));
        }
        Ok(true)
    }

    /// Whether this file carries id-bearing edges
    /// ([`EDGE_ID_FEATURE`], `docs/core/GRAPH_CONTRACT.md` §2.3).
    fn edge_identity_present(&self) -> bool {
        self.index_header
            .is_some_and(|h| h.features & EDGE_ID_FEATURE != 0)
    }

    /// The next edge id, advanced in memory; `commit` writes the allocator.
    fn allocate_edge_id(&mut self) -> Result<u64> {
        if self.supportive.is_some() {
            // A Register file reserves edge ids in blocks, as it does row
            // ids (`collections::ROW_ID_BLOCK`).
            let (next, mut reserved) = match (self.edge_id_next, self.edge_id_block) {
                (Some(next), Some((_, reserved))) => (next, reserved),
                (None, Some(block)) => block,
                _ if self.edge_identity_present() => {
                    let p = self
                        .entry_get(&crate::supportive::schema::edge_id_key(), &[])?
                        .ok_or_else(|| corrupt("edge-id allocator NEXT missing"))?;
                    let n = u64::from_be_bytes(p.try_into().map_err(|_| corrupt("edge-id allocator NEXT"))?);
                    (n, n)
                }
                _ => (1, 1),
            };
            if next >= reserved {
                reserved = next
                    .checked_add(crate::collections::ROW_ID_BLOCK)
                    .ok_or_else(|| invalid("graph edge identities exhausted"))?;
                self.edge_id_dirty = true;
            }
            let following = next
                .checked_add(1)
                .ok_or_else(|| invalid("graph edge identities exhausted"))?;
            self.edge_id_next = Some(following);
            self.edge_id_block = Some((following, reserved));
            return Ok(next);
        }
        let next = match self.edge_id_next {
            Some(next) => next,
            None if self.edge_identity_present() => {
                read_edge_id_allocator(|key| self.store()?.get(key).map_err(Error::from))?
            }
            // No id has ever been handed out in this file.
            None => 1,
        };
        let following = next
            .checked_add(1)
            .ok_or_else(|| invalid("graph edge identities exhausted"))?;
        self.edge_id_next = Some(following);
        self.edge_id_dirty = true;
        Ok(next)
    }

    /// Called by `commit`: the allocator's three replicas, once per commit
    /// that created an edge, in the same transaction as the edges.
    pub(crate) fn flush_edge_id_allocator(&mut self) -> Result<()> {
        if self.edge_id_dirty {
            let next = self
                .edge_id_next
                .ok_or_else(|| corrupt("edge-id allocator marked dirty without a value"))?;
            if let Some((_, reserved)) = self.edge_id_block.filter(|_| self.supportive.is_some()) {
                self.entry_put(
                    &crate::supportive::schema::edge_id_key(),
                    crate::supportive::schema::line(b"NEXT", 1, crate::supportive::schema::NEXT_EDGE_ID as u32),
                    &[],
                    &reserved.to_be_bytes(),
                )?;
            } else {
                let bytes = encode_edge_id_allocator(next)?;
                for copy in 0..3 {
                    self.writer()?.put(&edge_id_allocator_key(copy), &bytes)?;
                }
            }
            self.edge_id_dirty = false;
        }
        // Re-read at the next transaction's first create, as the entity
        // sequence is: the durable file is the authority between commits.
        self.edge_id_next = None;
        Ok(())
    }

    /// Create a NEW edge (`docs/core/GRAPH_CONTRACT.md` §2.3): always a new
    /// edge with a new id, even when an edge of the same
    /// `(source, context, type, destination)` already exists -- the two are
    /// PARALLEL edges and both are kept, each with its own properties.
    ///
    /// The id is allocated from one database-wide counter that only moves
    /// forward, so it is never handed out twice. The FIRST call on a file
    /// sets [`EDGE_ID_FEATURE`] in the same transaction as the first
    /// id-bearing key; the caller publishes both with `commit`.
    ///
    /// Cost (contract L4): the id segment on each of the two keys (2..=9
    /// bytes each), one allocator write per COMMIT rather than per edge, and
    /// one point probe of the new primary key so that a damaged allocator
    /// can never make this call overwrite an existing edge.
    pub fn create_edge(
        &mut self,
        context: GraphContextId,
        source: EntityId,
        edge_type: EdgeTypeId,
        destination: EntityId,
        properties: &Value,
    ) -> Result<EdgeId> {
        self.refuse_bound_edge_type(edge_type)?;
        self.create_edge_inner(context, source, edge_type, destination, properties)
    }

    /// `create_edge` for an edge table's own write (`edge_table.rs`).
    pub(crate) fn create_edge_typed(&mut self, key: EdgeKey, properties: &Value) -> Result<EdgeId> {
        self.create_edge_inner(key.context, key.source, key.edge_type, key.destination, properties)
    }

    fn create_edge_inner(
        &mut self,
        context: GraphContextId,
        source: EntityId,
        edge_type: EdgeTypeId,
        destination: EntityId,
        properties: &Value,
    ) -> Result<EdgeId> {
        self.user_write()?;
        let h = self.graph_header()?;
        let key = EdgeKey {
            source,
            context,
            edge_type,
            destination,
        };
        self.validate_edge_ids(h, key)?;
        self.validate_endpoints(source, destination)?;
        let bytes = encode_properties(properties)?;
        if self
            .limits
            .is_some_and(|l| bytes.len() + 64 > l.record_bytes as usize)
        {
            return Err(invalid("encoded edge exceeds configured record limit"));
        }
        let result = (|| {
            let id = self.allocate_edge_id()?;
            let primary = edge_key_id(PRIMARY_EDGE, key, id);
            if self.store()?.get(&primary)?.is_some() {
                return Err(corrupt(
                    "graph edge-id allocator is behind an existing edge id",
                ));
            }
            // Before the first key, like every additive bit here: the header
            // write and the keys are one transaction.
            self.enable_logical_feature(EDGE_ID_FEATURE)?;
            let maintain = self.endpoint_maintenance()?;
            self.writer()?.put(&primary, &bytes)?;
            self.writer()?
                .put(&edge_key_id(REVERSE_EDGE, key, id), &[])?;
            if maintain {
                // A new edge files both its ends; a put of a key that a
                // sibling edge already filed changes nothing.
                self.insert_endpoint_keys(key)?;
            }
            Ok(EdgeId { key, id })
        })();
        self.finish(result)
    }

    /// Replace ONE edge's property bag in place (contract §2.6): a rewrite of
    /// its primary posting, nothing else. Its parallel siblings, its mirror
    /// and the endpoint sets are untouched. Returns `false` when no such edge
    /// exists.
    pub fn update_edge_properties(&mut self, edge: EdgeId, properties: &Value) -> Result<bool> {
        self.refuse_bound_edge_type(edge.key.edge_type)?;
        self.update_edge_inner(edge, properties)
    }

    /// An edge table's own property rewrite, which has checked the bag
    /// against the table's columns (`edge_table.rs`).
    pub(crate) fn write_edge_bag(&mut self, edge: EdgeId, properties: &Value) -> Result<bool> {
        self.update_edge_inner(edge, properties)
    }

    fn update_edge_inner(&mut self, edge: EdgeId, properties: &Value) -> Result<bool> {
        self.user_write()?;
        let h = self.graph_header()?;
        self.validate_edge_ids(h, edge.key)?;
        let bytes = encode_properties(properties)?;
        if self
            .limits
            .is_some_and(|l| bytes.len() + 64 > l.record_bytes as usize)
        {
            return Err(invalid("encoded edge exceeds configured record limit"));
        }
        if !self.preflight_edge_id(edge.key, edge.id)? {
            return Ok(false);
        }
        let result = (|| {
            self.writer()?
                .put(&edge_key_id(PRIMARY_EDGE, edge.key, edge.id), &bytes)?;
            Ok(true)
        })();
        self.finish(result)
    }

    /// Delete ONE edge by its identity. Its parallel siblings stay, and so
    /// do the endpoint-set keys (§2.7) of any end that still has an edge of
    /// the same (context, type, direction): an end leaves its set only with
    /// its LAST such edge. Returns `false` when no such edge exists.
    pub fn delete_edge_by_id(&mut self, edge: EdgeId) -> Result<bool> {
        self.user_write()?;
        let h = self.graph_header()?;
        self.validate_edge_ids(h, edge.key)?;
        if !self.preflight_edge_id(edge.key, edge.id)? {
            return Ok(false);
        }
        let maintain = self.endpoint_sets_present();
        let result = (|| {
            if !self
                .writer()?
                .delete(&edge_key_id(PRIMARY_EDGE, edge.key, edge.id))?
                || !self
                    .writer()?
                    .delete(&edge_key_id(REVERSE_EDGE, edge.key, edge.id))?
            {
                return Err(corrupt("edge disappeared during delete"));
            }
            if maintain {
                self.remove_endpoint_keys_if_last(edge.key)?;
            }
            Ok(true)
        })();
        self.finish(result)
    }

    pub fn unlink(
        &mut self,
        source: EntityId,
        edge_type: &str,
        destination: EntityId,
        context: &str,
    ) -> Result<bool> {
        self.user_write()?;
        validate_name_input(edge_type, "edge type name")?;
        if !context.is_empty() {
            validate_name_input(context, "graph context name")?;
        }
        self.graph_header()?;
        let Some(edge_type) = self.lookup_graph_name(0, edge_type)?.map(EdgeTypeId) else {
            return Ok(false);
        };
        let context = if context.is_empty() {
            GraphContextId::BASE
        } else {
            let Some(id) = self.lookup_graph_name(1, context)?.map(GraphContextId) else {
                return Ok(false);
            };
            id
        };
        self.delete_edge(EdgeKey {
            source,
            context,
            edge_type,
            destination,
        })
    }

    fn validate_query_ids(
        &self,
        h: GraphHeader,
        context: GraphContextId,
        edge_type: Option<EdgeTypeId>,
    ) -> Result<()> {
        if context.0 >= h.next_context {
            return Err(invalid("unknown graph context identity"));
        }
        if edge_type.is_some_and(|id| id.0 == 0 || id.0 >= h.next_type) {
            return Err(invalid("unknown edge type identity"));
        }
        Ok(())
    }

    /// One direction of a neighbour read: every edge of `entity` in
    /// `direction` into `out`, keyed by its whole identity, with the bag an
    /// outgoing posting carries (its bytes counted against the call's
    /// bound). Stops once `out` holds more than `stop_after`.
    #[allow(clippy::too_many_arguments)]
    fn collect_direction(
        &self,
        h: GraphHeader,
        entity: EntityId,
        direction: Direction,
        context: GraphContextId,
        edge_type_id: Option<EdgeTypeId>,
        out: &mut BTreeMap<(EdgeKey, u64), Option<Vec<u8>>>,
        property_bytes: &mut usize,
        stop_after: usize,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        // The shared adjacency walk (`adjacency.rs`) hands out borrows into
        // the pinned leaf. The cancel poll keeps its old schedule exactly:
        // one per row the walk reads, the first row past the range included.
        let mut cursor =
            adjacency::AdjacencyCursor::at(self, h, entity, direction, context, edge_type_id)?;
        loop {
            let Some(posting) = cursor.next_posting()? else {
                return poll_past(&cursor, cancel);
            };
            poll(cancel)?;
            let adjacent = posting.edge()?;
            match adjacent.bag {
                // The scan already handed us the authoritative row, so the
                // properties are in hand: keep them and spend no lookup.
                Some(value) => {
                    *property_bytes = property_bytes
                        .checked_add(value.len())
                        .ok_or_else(|| invalid("neighbor property-byte bound overflow"))?;
                    if *property_bytes > MAX_NEIGHBOR_PROPERTY_BYTES {
                        return Err(invalid("neighbor properties exceed 1 MiB call bound"));
                    }
                    out.insert((adjacent.key, adjacent.id), Some(value.to_vec()));
                }
                None => {
                    out.entry((adjacent.key, adjacent.id)).or_insert(None);
                }
            }
            if out.len() > stop_after {
                return Ok(());
            }
        }
    }

    /// One direction of a keys-only neighbour walk: the far endpoint of every
    /// edge under the prefix, appended to `found`, with no property byte read
    /// and no authoritative-row lookup.
    fn collect_adjacent(
        &self,
        h: GraphHeader,
        request: NeighborRequest,
        direction: Direction,
        found: &mut Vec<EntityId>,
        scanned: &mut usize,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        let mut cursor = adjacency::AdjacencyCursor::at(
            self,
            h,
            request.entity,
            direction,
            request.context,
            request.edge_type,
        )?;
        loop {
            let Some(posting) = cursor.next_posting()? else {
                return poll_past(&cursor, cancel);
            };
            poll(cancel)?;
            *scanned = scanned
                .checked_add(1)
                .ok_or_else(|| invalid("neighbor edge work overflow"))?;
            found.push(posting.edge()?.far);
            if found.len() > request.limit {
                // A self-loop or a run of PARALLEL edges repeats an entity
                // inside one call. The normalise brings the list back to its
                // distinct entries, and the walk stops the moment the
                // DISTINCT count is genuinely over.
                found.sort_unstable();
                found.dedup();
                if found.len() > request.limit {
                    return Ok(());
                }
            }
        }
    }

    pub fn neighbors(&self, request: NeighborRequest) -> Result<Vec<Edge>> {
        self.neighbors_with_cancel(request, || false)
    }

    /// Complete-or-error neighbor read with cooperative cancellation.
    pub fn neighbors_with_cancel(
        &self,
        request: NeighborRequest,
        mut cancel: impl FnMut() -> bool,
    ) -> Result<Vec<Edge>> {
        if cancel() {
            return Err(invalid("graph query cancelled"));
        }
        if request.limit > MAX_NEIGHBORS {
            return Err(invalid("neighbor limit exceeds 256"));
        }
        let h = self.graph_header()?;
        self.validate_query_ids(h, request.context, request.edge_type)?;
        // Keyed by the edge's whole identity, so parallel edges are each an
        // entry and a self-loop walked in both directions is still one.
        let mut keys: BTreeMap<(EdgeKey, u64), Option<Vec<u8>>> = BTreeMap::new();
        let mut property_bytes = 0usize;
        if matches!(request.direction, Direction::Outgoing | Direction::Both) {
            self.collect_direction(
                h,
                request.entity,
                Direction::Outgoing,
                request.context,
                request.edge_type,
                &mut keys,
                &mut property_bytes,
                request.limit,
                &mut cancel,
            )?;
        }
        if keys.len() <= request.limit
            && matches!(request.direction, Direction::Incoming | Direction::Both)
        {
            self.collect_direction(
                h,
                request.entity,
                Direction::Incoming,
                request.context,
                request.edge_type,
                &mut keys,
                &mut property_bytes,
                request.limit,
                &mut cancel,
            )?;
        }
        // The seed-existence refusal, paid only when it can still be the
        // answer. An edge cannot outlive its endpoints -- a write validates
        // both and a delete cascades -- so an edge found under this prefix is
        // itself the proof that the seed's row is there. Only a seed with no
        // edge at all still needs the point lookup that every call used to
        // spend: measured at 528 ns on the 50K multimodel database, against a
        // 2.3 us outgoing one-hop.
        if keys.is_empty() && self.store()?.get(&row_key(request.entity))?.is_none() {
            return Err(Error::NotFound("graph endpoint"));
        }
        if keys.len() > request.limit {
            return Err(invalid(
                "neighbor result exceeds requested complete-result limit",
            ));
        }
        let mut out = Vec::with_capacity(keys.len());
        for ((key, id), collected) in keys {
            if cancel() {
                return Err(invalid("graph query cancelled"));
            }
            // A reverse key carries no properties, so an incoming edge still
            // costs the one read of its authoritative row -- and only one.
            let value = match collected {
                Some(value) => value,
                None => {
                    let value = adjacency::primary_posting(self, key, id)?;
                    property_bytes = property_bytes
                        .checked_add(value.len())
                        .ok_or_else(|| invalid("neighbor property-byte bound overflow"))?;
                    if property_bytes > MAX_NEIGHBOR_PROPERTY_BYTES {
                        return Err(invalid("neighbor properties exceed 1 MiB call bound"));
                    }
                    value
                }
            };
            out.push(Edge {
                key,
                id,
                properties: named_bag(self, key.edge_type, decode_properties(&value)?)?,
            });
        }
        Ok(out)
    }

    /// The adjacency alone: distinct entities one edge away, with no property
    /// byte decoded and, for an incoming walk, no authoritative-row lookup.
    ///
    /// [`Database::neighbors`] answers with whole [`Edge`]s. That is the right
    /// answer for a caller that wants the relationship, and the wrong price for
    /// one that wants the adjacency: an incoming read pays a point lookup of
    /// the primary row per edge purely to decode properties it is about to
    /// drop. The bound is the same complete-or-error bound, applied to the
    /// number of DISTINCT adjacent entities, and the result is sorted.
    pub fn neighbor_ids(&self, request: NeighborRequest) -> Result<Vec<EntityId>> {
        self.neighbor_ids_with_cancel(request, || false)
    }

    /// Complete-or-error keys-only neighbour read with cooperative cancellation.
    pub fn neighbor_ids_with_cancel(
        &self,
        request: NeighborRequest,
        mut cancel: impl FnMut() -> bool,
    ) -> Result<Vec<EntityId>> {
        if cancel() {
            return Err(invalid("graph query cancelled"));
        }
        if request.limit > MAX_NEIGHBORS {
            return Err(invalid("neighbor limit exceeds 256"));
        }
        let h = self.graph_header()?;
        self.validate_query_ids(h, request.context, request.edge_type)?;
        let mut found: Vec<EntityId> = Vec::new();
        let mut scanned = 0usize;
        if matches!(request.direction, Direction::Outgoing | Direction::Both) {
            self.collect_adjacent(
                h,
                request,
                Direction::Outgoing,
                &mut found,
                &mut scanned,
                &mut cancel,
            )?;
        }
        if matches!(request.direction, Direction::Incoming | Direction::Both) {
            self.collect_adjacent(
                h,
                request,
                Direction::Incoming,
                &mut found,
                &mut scanned,
                &mut cancel,
            )?;
        }
        found.sort_unstable();
        found.dedup();
        // Same last-resort rule as `neighbors`: one edge seen is proof enough.
        if scanned == 0 && self.store()?.get(&row_key(request.entity))?.is_none() {
            return Err(Error::NotFound("graph endpoint"));
        }
        if found.len() > request.limit {
            return Err(invalid(
                "neighbor result exceeds requested complete-result limit",
            ));
        }
        Ok(found)
    }

    /// Deterministic distinct-entity BFS. Budget exhaustion is an error; the
    /// returned vector therefore always represents a complete bounded result.
    pub fn traverse_bfs(&self, request: BfsRequest<'_>) -> Result<TraversalResult> {
        self.traverse_bfs_with_cancel(request, || false)
    }

    /// The same walk, binding the edge that reached each node
    /// (`docs/core/GRAPH_CONTRACT.md` §4.2): every returned [`TraversalNode`]
    /// carries `via`, except the seed, which no edge reached.
    pub fn traverse_bfs_binding_edges(&self, request: BfsRequest<'_>) -> Result<TraversalResult> {
        self.traverse_bfs_inner(request, true, || false)
    }

    /// Complete-or-error deterministic BFS with cooperative cancellation.
    pub fn traverse_bfs_with_cancel(
        &self,
        request: BfsRequest<'_>,
        cancel: impl FnMut() -> bool,
    ) -> Result<TraversalResult> {
        self.traverse_bfs_inner(request, false, cancel)
    }

    fn traverse_bfs_inner(
        &self,
        request: BfsRequest<'_>,
        wants_via: bool,
        mut cancel: impl FnMut() -> bool,
    ) -> Result<TraversalResult> {
        if cancel() {
            return Err(invalid("graph query cancelled"));
        }
        if request.min_depth > request.max_depth
            || request.max_depth > MAX_BFS_DEPTH
            || request.max_visited == 0
            || request.max_visited > MAX_BFS_VISITED
            || request.max_edges == 0
            || request.max_edges > MAX_BFS_EDGES
            || request.result_limit > MAX_BFS_RESULTS
        {
            return Err(invalid("invalid BFS depth/work/result bounds"));
        }
        let h = self.graph_header()?;
        self.validate_query_ids(h, request.context, request.edge_type)?;
        // The visited set is a SORTED VECTOR, not a `BTreeSet`. Membership is
        // asked once per edge and the answer is a binary search either way,
        // but the set also grows by a whole level at a time -- and each level
        // arrives already sorted and already disjoint from what has been seen,
        // because an entity that is seen is never offered. That makes the
        // union one linear merge instead of one tree insert per entity.
        // Measured on the 500-edge organization fan-in of the 50K multimodel
        // database: 39.5 ns per edge through `BTreeSet`, 18.1 ns through this.
        let mut seen = vec![request.seed];
        let mut merged: Vec<EntityId> = Vec::new();
        if seen.len() > request.max_visited {
            return Err(invalid("BFS visited limit exceeded"));
        }
        let mut nodes = Vec::new();
        if request.include_seed && request.min_depth == 0 {
            if request.result_limit == 0 {
                return Err(invalid("BFS result limit exceeded"));
            }
            nodes.push(TraversalNode {
                entity: request.seed,
                depth: 0,
                via: None,
            });
        }
        // The node predicates, compiled and their sets walked ONCE before the
        // first hop. A refused filter kind fails here, not per node.
        let gate = crate::query::StandaloneNodeGate::new(self, request.node_where)?;
        let hop = Hop {
            header: h,
            context: request.context,
            edge_type: request.edge_type,
            edge_where: request.edge_where,
            wants_via,
            max_edges: request.max_edges,
        };
        let mut work = BfsWork {
            db: self,
            gate: &gate,
            refused: BTreeSet::new(),
            cancel,
        };
        let mut frontier = vec![request.seed];
        let mut scanned_edges = 0usize;
        for depth in 1..=request.max_depth {
            work.poll()?;
            let mut next = Frontier::new(request.max_visited - seen.len());
            for entity in frontier {
                work.poll()?;
                for direction in [Direction::Outgoing, Direction::Incoming] {
                    if matches!(request.direction, Direction::Both) || request.direction == direction {
                        hop.walk(self, entity, direction, &seen, &mut next, &mut scanned_edges, &mut work)?;
                    }
                }
            }
            let level = next.into_sorted();
            if seen.len() + level.len() > request.max_visited {
                return Err(invalid("BFS visited limit exceeded"));
            }
            let ids: Vec<EntityId> = level.iter().map(|entry| entry.entity).collect();
            merge_sorted_disjoint(&mut seen, &ids, &mut merged);
            if depth >= request.min_depth {
                if nodes.len() + level.len() > request.result_limit {
                    return Err(invalid("BFS result limit exceeded"));
                }
                nodes.extend(level.iter().map(|entry| TraversalNode {
                    entity: entry.entity,
                    depth,
                    via: entry.via.as_ref().map(|via| EdgeRef::clone(via)),
                }));
            }
            if ids.is_empty() {
                break;
            }
            frontier = ids;
        }
        // The seed-existence refusal, paid only when it can still be the
        // answer: a traversal that walked even one edge has already proved the
        // seed's row is there, because an edge cannot outlive its endpoints.
        if scanned_edges == 0 && self.store()?.get(&row_key(request.seed))?.is_none() {
            return Err(Error::NotFound("graph endpoint"));
        }
        Ok(TraversalResult {
            nodes,
            visited: seen.len(),
            scanned_edges,
        })
    }

    fn preflight_incident_edges(&self, entity: EntityId) -> Result<Vec<(EdgeKey, u64)>> {
        if !self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0)
        {
            return Ok(Vec::new());
        }
        let h = self.graph_header()?;
        let mut edges = BTreeSet::new();
        for tag in [PRIMARY_EDGE, REVERSE_EDGE] {
            let p = edge_prefix(tag, entity, None, None);
            for row in self.store()?.range(&p)? {
                let (key, value) = row?;
                if !key.starts_with(&p) {
                    break;
                }
                let (edge, id) = parse_edge(&key, tag)?;
                self.validate_stored_edge_ids(h, edge)?;
                if tag == PRIMARY_EDGE {
                    decode_properties(&value)?;
                    if self.store()?.get(&edge_key_id(REVERSE_EDGE, edge, id))?.as_deref()
                        != Some(&[])
                    {
                        return Err(corrupt("missing/nonempty reverse edge marker"));
                    }
                } else {
                    if !value.is_empty() {
                        return Err(corrupt("nonempty reverse edge marker"));
                    }
                    let primary = self
                        .store()?
                        .get(&edge_key_id(PRIMARY_EDGE, edge, id))?
                        .ok_or_else(|| corrupt("reverse edge without authoritative primary"))?;
                    decode_properties(&primary)?;
                }
                // Each PARALLEL edge is its own entry and counts against the
                // cascade bound on its own.
                edges.insert((edge, id));
                if edges.len() > MAX_CASCADE {
                    return Err(invalid("entity has more than 256 incident graph edges"));
                }
            }
        }
        Ok(edges.into_iter().collect())
    }

    /// Every graph context that holds an edge incident on a row of `c`, and
    /// whether the probe stopped at its cap before it could say it had them
    /// all.
    ///
    /// This is GRAPH_CONTRACT 6.1's RESTRICT question asked of a whole
    /// collection rather than of one node. The edge key is
    /// `tag | collection | sequence | context | type | far endpoint`, so
    /// `tag | collection` is one contiguous range holding every edge incident
    /// on any row of the collection in any context -- one range probe per tag,
    /// not one per context, because a context cannot be a key prefix here.
    ///
    /// Law 1: the walk is not proportional to the edges. After the first edge
    /// of one (entity, context) run it SEEKS past the run to
    /// `tag | entity | context + 1`, so it pays one descent per distinct
    /// (entity, context) pair that has edges and stops at `cap` of them. A
    /// collection with no edges costs two descents.
    pub(crate) fn collection_edge_contexts(
        &self,
        c: CollectionId,
        cap: usize,
    ) -> Result<(Vec<GraphContextId>, bool)> {
        if !self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0)
        {
            return Ok((Vec::new(), false));
        }
        let h = self.graph_header()?;
        let mut found = BTreeSet::new();
        let mut truncated = false;
        for tag in [PRIMARY_EDGE, REVERSE_EDGE] {
            let p = crate::collections::prefix(tag, c);
            let mut from = p.clone();
            let mut seeks = 0usize;
            loop {
                if seeks >= cap {
                    truncated = true;
                    break;
                }
                seeks += 1;
                let Some(row) = self.store()?.range(&from)?.next() else {
                    break;
                };
                let (key, _) = row?;
                if !key.starts_with(&p) {
                    break;
                }
                let (edge, _) = parse_edge(&key, tag)?;
                self.validate_stored_edge_ids(h, edge)?;
                found.insert(edge.context);
                let near = if tag == PRIMARY_EDGE {
                    edge.source
                } else {
                    edge.destination
                };
                let Some(next) = edge.context.0.checked_add(1) else {
                    break;
                };
                from = edge_prefix(tag, near, Some(GraphContextId(next)), None);
            }
        }
        Ok((found.into_iter().collect(), truncated))
    }

    /// Every graph context that holds an edge incident on ONE row, and
    /// whether the probe stopped at its cap.
    ///
    /// GRAPH_CONTRACT 6.1's RESTRICT question asked of a node, which is the
    /// granularity the contract states it at and the granularity
    /// `collection_edge_contexts` (the same question asked of a collection)
    /// could not reach. Here the edge key IS prefixed by the entity --
    /// `tag | collection | sequence | context | type | far endpoint` -- so
    /// `tag | entity` is one contiguous range and a context is a prefix
    /// INSIDE it. Law 1: after the first edge of a context run the walk seeks
    /// past the run to `tag | entity | context + 1`, so it pays one descent
    /// per context that has edges, not one per edge, and two descents for a
    /// row with none.
    pub(crate) fn entity_edge_contexts(
        &self,
        entity: EntityId,
        cap: usize,
    ) -> Result<(Vec<GraphContextId>, bool)> {
        if !self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0)
        {
            return Ok((Vec::new(), false));
        }
        let h = self.graph_header()?;
        let mut found = BTreeSet::new();
        let mut truncated = false;
        for tag in [PRIMARY_EDGE, REVERSE_EDGE] {
            let p = edge_prefix(tag, entity, None, None);
            let mut from = p.clone();
            let mut seeks = 0usize;
            loop {
                if seeks >= cap {
                    truncated = true;
                    break;
                }
                seeks += 1;
                let Some(row) = self.store()?.range(&from)?.next() else {
                    break;
                };
                let (key, _) = row?;
                if !key.starts_with(&p) {
                    break;
                }
                let (edge, _) = parse_edge(&key, tag)?;
                self.validate_stored_edge_ids(h, edge)?;
                found.insert(edge.context);
                let Some(next) = edge.context.0.checked_add(1) else {
                    break;
                };
                from = edge_prefix(tag, entity, Some(GraphContextId(next)), None);
            }
        }
        Ok((found.into_iter().collect(), truncated))
    }

    /// The name an edge type was interned under: an edge table's label.
    pub fn edge_type_name(&self, id: EdgeTypeId) -> Result<String> {
        self.graph_name_of(0, id.0)
    }

    /// An interned edge-type (kind 0) or context (kind 1) name by id.
    pub(crate) fn graph_name_of(&self, kind: u8, id: u64) -> Result<String> {
        if let Some(sup) = &self.supportive {
            return crate::collections::register_catalog::graph_name(self.store()?, sup, kind, id)?
                .ok_or_else(|| corrupt("all metadata copies missing or damaged"));
        }
        Ok(read_name(|key| self.store()?.get(key).map_err(Error::from), kind, id)?.name)
    }

    /// The name a context was interned under, for a refusal that has to name
    /// it. The base graph has no descriptor and no name.
    pub(crate) fn graph_context_name(&self, id: GraphContextId) -> Result<String> {
        if id == GraphContextId::BASE {
            return Ok("(base graph)".to_owned());
        }
        self.graph_name_of(1, id.0)
    }

    /// Parent `Database::delete` calls this before any entity/index mutation.
    /// The preflight reads at most 257 distinct incident tuples. Only after it
    /// succeeds are complete primary/reverse pairs removed. A checksum-valid
    /// omitted reverse marker cannot prove that no incoming primary exists;
    /// missing authoritative properties are never reconstructed from markers.
    pub(crate) fn cascade_graph_delete(&mut self, entity: EntityId) -> Result<()> {
        let edges = self.preflight_incident_edges(entity)?;
        let result = (|| {
            for (edge, id) in &edges {
                if !self.writer()?.delete(&edge_key_id(PRIMARY_EDGE, *edge, *id))?
                    || !self.writer()?.delete(&edge_key_id(REVERSE_EDGE, *edge, *id))?
                {
                    return Err(corrupt("incident edge disappeared during cascade"));
                }
            }
            let edges: Vec<EdgeKey> = edges.iter().map(|(edge, _)| *edge).collect();
            // Every incident edge is gone before a single endpoint key is
            // probed, so the deleted entity's own keys all go and each
            // neighbour's goes only if this cascade took its last edge. The
            // probes are deduplicated: a node with 200 incident edges of one
            // type probes its own end once, not 200 times.
            self.remove_endpoint_keys_for(&edges)?;
            Ok(())
        })();
        self.finish(result)
    }
}

#[cfg(test)]
#[path = "../../faults/graph_fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
mod empty_properties_tests {
    use super::*;

    #[test]
    fn empty_properties_are_what_the_writer_writes() {
        let mut written = vec![1u8];
        written.extend(crate::binary_json(&serde_json::json!({})).unwrap());
        assert_eq!(written, EMPTY_PROPERTIES, "the empty-object encoding moved out from under encode_properties");
        assert_eq!(encode_properties(&serde_json::json!({})).unwrap(), EMPTY_PROPERTIES);
    }
}
