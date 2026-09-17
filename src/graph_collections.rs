//! Bounded graph relationships over the collection page-WAL.
//!
//! Primary edge rows are authoritative user data. Reverse rows are mandatory
//! navigation markers and cannot recover missing primary properties.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const GRAPH_FEATURE: u64 = 2;
pub(super) const GRAPH_HEADER: u8 = 0x06;
pub(super) const NAME_DESCRIPTOR: u8 = 0x07;
pub(super) const NAME_LOOKUP: u8 = 0x12;
pub(super) const PRIMARY_EDGE: u8 = 0x71;
pub(super) const REVERSE_EDGE: u8 = 0x72;
const GRAPH_MAGIC: &[u8; 8] = b"E4GRF01\0";
const NAME_MAGIC: &[u8; 8] = b"E4GNM01\0";
const GRAPH_ENCODING: u16 = 1;
const REVERSE_REQUIRED: u16 = 1;
const MAX_NAMES: u32 = 4096;
const MAX_NAME_BYTES: usize = 128;
const MAX_EDGE_PROPERTY_BYTES: usize = 64 * 1024;
const MAX_NEIGHBORS: usize = 256;
const MAX_NEIGHBOR_PROPERTY_BYTES: usize = 1024 * 1024;
const MAX_BFS_DEPTH: usize = 64;
const MAX_BFS_VISITED: usize = 65_536;
const MAX_BFS_EDGES: usize = 1_000_000;
const MAX_BFS_RESULTS: usize = 65_536;
const MAX_CASCADE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeTypeId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphContextId(pub u64);
impl GraphContextId {
    pub const BASE: Self = Self(0);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeKey {
    pub source: EntityId,
    pub context: GraphContextId,
    pub edge_type: EdgeTypeId,
    pub destination: EntityId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub key: EdgeKey,
    pub properties: Value,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BfsRequest {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraversalNode {
    pub entity: EntityId,
    pub depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraversalResult {
    pub nodes: Vec<TraversalNode>,
    pub visited: usize,
    pub scanned_edges: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct GraphHeader {
    pub(super) next_type: u64,
    pub(super) next_context: u64,
    pub(super) type_count: u32,
    pub(super) context_count: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct GraphName {
    pub(super) kind: u8,
    pub(super) id: u64,
    pub(super) name: String,
}

pub(super) fn graph_header_key(copy: u8) -> Vec<u8> {
    vec![GRAPH_HEADER, copy]
}

pub(super) fn name_descriptor_key(kind: u8, copy: u8, id: u64) -> Vec<u8> {
    let mut key = vec![NAME_DESCRIPTOR, kind, copy];
    key.extend(ordered(id));
    key
}

pub(super) fn name_lookup_key(kind: u8, name: &str) -> Vec<u8> {
    let mut key = vec![NAME_LOOKUP, kind];
    key.extend_from_slice(name.as_bytes());
    key
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

pub(super) fn decode_graph_header(bytes: &[u8]) -> Result<GraphHeader> {
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

pub(super) fn read_graph_header(
    get: impl FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<GraphHeader> {
    replicas(get, graph_header_key, decode_graph_header)
}

fn encode_name(n: &GraphName) -> Result<Vec<u8>> {
    let mut body = vec![n.kind];
    body.extend(n.id.to_be_bytes());
    body.extend((n.name.len() as u16).to_be_bytes());
    body.extend_from_slice(n.name.as_bytes());
    packet(NAME_MAGIC, &body)
}

pub(super) fn decode_name(bytes: &[u8]) -> Result<GraphName> {
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

pub(super) fn read_name(
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
    key.extend(ordered(id.collection.0.into()));
    key.extend(ordered(id.sequence));
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

pub(super) fn edge_key(tag: u8, edge: EdgeKey) -> Vec<u8> {
    let mut key = vec![tag];
    let (first, last) = if tag == PRIMARY_EDGE {
        (edge.source, edge.destination)
    } else {
        (edge.destination, edge.source)
    };
    append_entity(&mut key, first);
    key.extend(ordered(edge.context.0));
    key.extend(ordered(edge.edge_type.0));
    append_entity(&mut key, last);
    key
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
pub(super) const MAX_EDGE_PREFIX: usize = 1 + 9 + 9 + 9 + 9;

/// The same prefix, written into the caller's stack buffer.
///
/// A one-hop read is a few hundred nanoseconds of real work, and it built this
/// eleven-byte key with a heap allocation -- once per direction, on every call.
/// The encoding is the frozen one; the debug assertion below holds it to
/// `edge_prefix` byte for byte, so the two can never drift apart unnoticed.
pub(super) fn edge_prefix_into(
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

pub(super) fn parse_edge_key(key: &[u8], tag: u8) -> Result<EdgeKey> {
    if key.first() != Some(&tag) {
        return Err(corrupt("graph edge key tag"));
    }
    let mut at = 1;
    let first = read_entity(key, &mut at)?;
    let context = GraphContextId(read_ordered(key, &mut at)?);
    let edge_type = EdgeTypeId(read_ordered(key, &mut at)?);
    let last = read_entity(key, &mut at)?;
    if at != key.len() || edge_type.0 == 0 {
        return Err(corrupt("graph edge key fields"));
    }
    Ok(if tag == PRIMARY_EDGE {
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
    })
}

/// The far endpoint of an edge key that a prefix scan just handed us.
///
/// `parse_edge_key` reads all four fields back out of the key. A prefix scan
/// has already fixed three of them -- the near entity, the context, and, when
/// the caller named one, the edge type -- byte for byte: the `starts_with`
/// test proved the row carries exactly the bytes the scan built. Only the far
/// entity is news, and for a traversal it is the whole answer.
///
/// Measured on the 500-edge organization fan-in of the 50K multimodel
/// database: 32.7 ns per edge for `parse_edge_key`, 12.0 ns for this.
pub(super) fn adjacent_from_tail(
    key: &[u8],
    at0: usize,
    pinned_type: Option<EdgeTypeId>,
    context: GraphContextId,
    h: GraphHeader,
) -> Result<(EdgeTypeId, EntityId)> {
    let mut at = at0;
    let edge_type = match pinned_type {
        Some(id) => id,
        None => EdgeTypeId(read_ordered(key, &mut at)?),
    };
    let adjacent = read_entity(key, &mut at)?;
    if at != key.len() {
        return Err(corrupt("graph edge key fields"));
    }
    // The guard `validate_stored_edge_ids` applied, on the same two fields.
    // The context reached us through the prefix the caller already validated;
    // the type came either from there or from the key we just read.
    if edge_type.0 == 0 || edge_type.0 >= h.next_type || context.0 >= h.next_context {
        return Err(corrupt("stored edge has unknown type/context identity"));
    }
    Ok((edge_type, adjacent))
}

/// The encoded form of `{}`. Nearly every edge in a graph carries no
/// properties at all, and decoding that costs a reader run and a map. The
/// encoder is the authority on these three bytes and `encode_properties`
/// asserts they stay in step.
const EMPTY_PROPERTIES: &[u8] = &[1, 8, 0];

fn encode_properties(value: &Value) -> Result<Vec<u8>> {
    if !value.is_object() {
        return Err(invalid("edge properties must be an object"));
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

pub(super) fn decode_properties(bytes: &[u8]) -> Result<Value> {
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
pub(super) struct Frontier {
    items: Vec<EntityId>,
    sorted: bool,
    room: usize,
}

impl Frontier {
    pub(super) fn new(room: usize) -> Self {
        Self {
            items: Vec::new(),
            sorted: true,
            room,
        }
    }

    pub(super) fn offer(&mut self, entity: EntityId) -> Result<()> {
        if self.sorted && self.items.len() == self.room {
            // Full and normalised: one more distinct entity is one too many.
            return if self.items.binary_search(&entity).is_ok() {
                Ok(())
            } else {
                Err(invalid("BFS visited limit exceeded"))
            };
        }
        self.items.push(entity);
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
            self.items.sort_unstable();
            self.items.dedup();
            self.sorted = true;
        }
    }

    pub(super) fn into_sorted(mut self) -> Vec<EntityId> {
        self.normalize();
        self.items
    }
}

/// Union of two sorted sets with no element in common, in one pass, reusing
/// the caller's scratch buffer so a deep traversal allocates per LEVEL rather
/// than per entity.
pub(super) fn merge_sorted_disjoint(
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

fn validate_name_input(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(invalid(format!("{what} requires 1..128 UTF-8 bytes")));
    }
    Ok(())
}

/// Bounded admission of graph metadata. Relationship rows deliberately are
/// not scanned here; their exact key/value and mirror are checked when read.
pub(super) fn validate_graph(s: &PageWalStore, enabled: bool) -> Result<()> {
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
    pub(super) fn graph_header(&self) -> Result<GraphHeader> {
        let enabled = self
            .index_header
            .is_some_and(|header| header.features & GRAPH_FEATURE != 0);
        if !enabled {
            return Err(invalid("graph feature is not enabled"));
        }
        if let Some(cached) = self.graph_header_cache.get() {
            return Ok(cached);
        }
        let h = read_graph_header(|key| self.store()?.get(key).map_err(Error::from))?;
        self.graph_header_cache.set(Some(h));
        Ok(h)
    }

    fn save_graph_header(&mut self, h: GraphHeader) -> Result<()> {
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
        for id in [source, destination] {
            if id.collection.0 == 0 || id.sequence == 0 {
                return Err(Error::NotFound("graph endpoint"));
            }
            // The existence check stays -- an edge to a row that is not there
            // is the dangling reference this guard exists to refuse. It is the
            // DESCENT that goes away, and only for a row this handle wrote and
            // has not deleted, where the answer is already known.
            if self.endpoint_known_live(id) {
                continue;
            }
            if self.store()?.get(&row_key(id))?.is_none() {
                return Err(Error::NotFound("graph endpoint"));
            }
        }
        Ok(())
    }

    fn preflight_edge_pair(&self, key: EdgeKey) -> Result<()> {
        let primary = self.store()?.get(&edge_key(PRIMARY_EDGE, key))?;
        let reverse = self.store()?.get(&edge_key(REVERSE_EDGE, key))?;
        match (primary, reverse) {
            (Some(value), Some(marker)) => {
                decode_properties(&value)?;
                if !marker.is_empty() {
                    return Err(corrupt("nonempty reverse edge marker"));
                }
            }
            (None, None) => {}
            _ => return Err(corrupt("graph primary/reverse mismatch")),
        }
        Ok(())
    }

    /// Both halves of an edge, written immediately.
    ///
    /// DEFERRED, with the reason recorded so it is not re-proposed blind:
    /// buffering the derived halves of a batch and flushing each tag as one
    /// ascending run at commit. Two findings stop it.
    ///
    /// * It would not arm the append fast path. `fast_path_leaf`
    ///   (kernel/src/btree.rs:1293-1316) accepts a hinted leaf only when
    ///   `next_leaf() == 0` -- rightmost in the WHOLE tree, not in its tag --
    ///   and there is one tree (kernel/src/store.rs:339). Only the highest tag
    ///   present can ever satisfy that, so sorting `0x71`/`0x72` into runs
    ///   still pays a full descent per write. Relaxing that check needs a
    ///   right-hand bound the hint can trust across a neighbour's growth;
    ///   getting it wrong appends keys that a scan finds and a `get` does not.
    /// * Deferred entries would be invisible to same-transaction readers.
    ///   `preflight_edge_pair`, `delete_edge`, `neighbors`, `bfs` and
    ///   `remove_node` (here) and the traversal in `src/query.rs` all read the
    ///   edge keyspace through `&self`, several by range scan, so they cannot
    ///   flush a buffer and cannot cheaply merge one. Correct writes that a
    ///   read in the same transaction cannot see are not a trade this engine
    ///   makes.
    ///
    /// So the edge family stays immediate, and the read side is what got
    /// cheaper instead -- see `preflight_unless_provably_absent`.
    fn write_edge_pair(&mut self, key: EdgeKey, properties: &[u8]) -> Result<()> {
        self.writer()?
            .put(&edge_key(PRIMARY_EDGE, key), properties)?;
        self.writer()?.put(&edge_key(REVERSE_EDGE, key), &[])?;
        self.note_edge_written(key);
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
    /// record depends on them, and `index_verifier` still finds a mismatch it
    /// can reach. What is lost is an early warning, not a repair.
    fn preflight_unless_provably_absent(&self, key: EdgeKey) -> Result<()> {
        if self.edge_provably_absent(key) {
            return Ok(());
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
        self.preflight_unless_provably_absent(key)?;
        let result = self.write_edge_pair(key, &bytes).map(|()| key);
        self.finish(result)
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
        self.preflight_unless_provably_absent(key)?;
        let result = (|| {
            for name in &names {
                self.save_graph_name(name)?;
            }
            if !names.is_empty() {
                self.save_graph_header(h)?;
            }
            self.write_edge_pair(key, &bytes)?;
            Ok(key)
        })();
        self.finish(result)
    }

    pub fn delete_edge(&mut self, key: EdgeKey) -> Result<bool> {
        self.user_write()?;
        let h = self.graph_header()?;
        self.validate_edge_ids(h, key)?;
        let primary_key = edge_key(PRIMARY_EDGE, key);
        let reverse_key = edge_key(REVERSE_EDGE, key);
        let primary = self.store()?.get(&primary_key)?;
        let reverse = self.store()?.get(&reverse_key)?;
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
        let result = (|| {
            if !self.writer()?.delete(&primary_key)? || !self.writer()?.delete(&reverse_key)? {
                return Err(corrupt("edge disappeared during delete"));
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

    fn collect_direction(
        &self,
        h: GraphHeader,
        entity: EntityId,
        direction: Direction,
        context: GraphContextId,
        edge_type_id: Option<EdgeTypeId>,
        out: &mut BTreeMap<EdgeKey, Option<Vec<u8>>>,
        property_bytes: &mut usize,
        stop_after: usize,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        let outgoing = direction == Direction::Outgoing;
        let tag = match direction {
            Direction::Outgoing => PRIMARY_EDGE,
            Direction::Incoming => REVERSE_EDGE,
            Direction::Both => unreachable!(),
        };
        let mut prefix = [0u8; MAX_EDGE_PREFIX];
        let at0 = edge_prefix_into(&mut prefix, tag, entity, Some(context), edge_type_id);
        let p = &prefix[..at0];
        // `for_each_ref` hands the callback borrows into the pinned leaf. The
        // allocating iterator built a key `Vec` per edge for a parser that
        // only reads it, and a value `Vec` per edge that an incoming read
        // discards unread. The cancel poll keeps its old schedule exactly:
        // one per row the cursor produces, before the prefix test.
        let mut failure: Option<Error> = None;
        {
            let mut step = |key: &[u8], value: &[u8]| -> Result<bool> {
                if cancel() {
                    return Err(invalid("graph query cancelled"));
                }
                if !key.starts_with(p) {
                    return Ok(false);
                }
                let (edge_type, adjacent) = adjacent_from_tail(key, at0, edge_type_id, context, h)?;
                let edge = if outgoing {
                    EdgeKey { source: entity, context, edge_type, destination: adjacent }
                } else {
                    EdgeKey { source: adjacent, context, edge_type, destination: entity }
                };
                if outgoing {
                    // The scan already handed us the authoritative row, so the
                    // properties are in hand: keep them and spend no lookup.
                    *property_bytes = property_bytes
                        .checked_add(value.len())
                        .ok_or_else(|| invalid("neighbor property-byte bound overflow"))?;
                    if *property_bytes > MAX_NEIGHBOR_PROPERTY_BYTES {
                        return Err(invalid("neighbor properties exceed 1 MiB call bound"));
                    }
                    out.insert(edge, Some(value.to_vec()));
                } else {
                    if !value.is_empty() {
                        return Err(corrupt("nonempty reverse edge marker"));
                    }
                    out.entry(edge).or_insert(None);
                }
                Ok(out.len() <= stop_after)
            };
            self.store()?
                .range(p)?
                .for_each_ref(|key, value| match step(key, value) {
                    Ok(keep_going) => keep_going,
                    Err(error) => {
                        failure = Some(error);
                        false
                    }
                })?;
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
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
        let incoming = direction == Direction::Incoming;
        let tag = if incoming { REVERSE_EDGE } else { PRIMARY_EDGE };
        let mut prefix = [0u8; MAX_EDGE_PREFIX];
        let at0 =
            edge_prefix_into(&mut prefix, tag, request.entity, Some(request.context), request.edge_type);
        let p = &prefix[..at0];
        let (pinned, context, limit) = (request.edge_type, request.context, request.limit);
        let mut failure: Option<Error> = None;
        {
            let mut step = |key: &[u8], value: &[u8]| -> Result<bool> {
                if cancel() {
                    return Err(invalid("graph query cancelled"));
                }
                if !key.starts_with(p) {
                    return Ok(false);
                }
                *scanned = scanned
                    .checked_add(1)
                    .ok_or_else(|| invalid("neighbor edge work overflow"))?;
                if incoming && !value.is_empty() {
                    return Err(corrupt("nonempty reverse edge marker"));
                }
                let (_, adjacent) = adjacent_from_tail(key, at0, pinned, context, h)?;
                found.push(adjacent);
                if found.len() > limit {
                    // Only a self-loop can repeat inside one call, so this
                    // normalise runs at most once per direction, and the walk
                    // stops the moment the DISTINCT count is genuinely over.
                    found.sort_unstable();
                    found.dedup();
                    if found.len() > limit {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            self.store()?
                .range(p)?
                .for_each_ref(|key, value| match step(key, value) {
                    Ok(keep_going) => keep_going,
                    Err(error) => {
                        failure = Some(error);
                        false
                    }
                })?;
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
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
        let mut keys: BTreeMap<EdgeKey, Option<Vec<u8>>> = BTreeMap::new();
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
        for (key, collected) in keys {
            if cancel() {
                return Err(invalid("graph query cancelled"));
            }
            // A reverse key carries no properties, so an incoming edge still
            // costs the one read of its authoritative row -- and only one.
            let value = match collected {
                Some(value) => value,
                None => {
                    let value = self
                        .store()?
                        .get(&edge_key(PRIMARY_EDGE, key))?
                        .ok_or_else(|| corrupt("edge disappeared during neighbor read"))?;
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
                properties: decode_properties(&value)?,
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

    fn bfs_direction(
        &self,
        h: GraphHeader,
        entity: EntityId,
        direction: Direction,
        request: &BfsRequest,
        next: &mut Frontier,
        seen: &[EntityId],
        scanned: &mut usize,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        let incoming = direction == Direction::Incoming;
        let tag = if incoming { REVERSE_EDGE } else { PRIMARY_EDGE };
        let mut prefix = [0u8; MAX_EDGE_PREFIX];
        let at0 =
            edge_prefix_into(&mut prefix, tag, entity, Some(request.context), request.edge_type);
        let p = &prefix[..at0];
        let (pinned, context, max_edges) = (request.edge_type, request.context, request.max_edges);
        // `for_each_ref` hands the callback borrows into the pinned leaf. The
        // allocating iterator built a key `Vec` and a value `Vec` for every
        // edge walked, to hand a parser bytes it only reads and a marker check
        // one byte of length. A traversal is a scan; it should cost the pages.
        //
        // The whole per-edge body lives here rather than in a helper: it used
        // to take the 88-byte `BfsRequest` by value, so every edge paid for a
        // copy of ten fields to read one of them.
        let mut failure: Option<Error> = None;
        {
            let mut step = |key: &[u8], value: &[u8]| -> Result<bool> {
                if !key.starts_with(p) {
                    return Ok(false);
                }
                if cancel() {
                    return Err(invalid("graph query cancelled"));
                }
                *scanned = scanned
                    .checked_add(1)
                    .ok_or_else(|| invalid("BFS edge work overflow"))?;
                if *scanned > max_edges {
                    return Err(invalid("BFS edge work limit exceeded"));
                }
                // BFS returns entities, so it never decodes properties, and it
                // never reads across to the other direction of the pair: both
                // directions are written in one transaction, so a committed
                // snapshot cannot hold half a pair, and `verify_indexed_source`
                // is the tool that checks pair consistency.
                if incoming && !value.is_empty() {
                    return Err(corrupt("nonempty reverse edge marker"));
                }
                let (_, adjacent) = adjacent_from_tail(key, at0, pinned, context, h)?;
                if seen.binary_search(&adjacent).is_err() {
                    next.offer(adjacent)?;
                }
                Ok(true)
            };
            self.store()?
                .range(p)?
                .for_each_ref(|key, value| match step(key, value) {
                    Ok(keep_going) => keep_going,
                    Err(error) => {
                        failure = Some(error);
                        false
                    }
                })?;
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    /// Deterministic distinct-entity BFS. Budget exhaustion is an error; the
    /// returned vector therefore always represents a complete bounded result.
    pub fn traverse_bfs(&self, request: BfsRequest) -> Result<TraversalResult> {
        self.traverse_bfs_with_cancel(request, || false)
    }

    /// Complete-or-error deterministic BFS with cooperative cancellation.
    pub fn traverse_bfs_with_cancel(
        &self,
        request: BfsRequest,
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
            });
        }
        let mut frontier = vec![request.seed];
        let mut scanned_edges = 0usize;
        for depth in 1..=request.max_depth {
            if cancel() {
                return Err(invalid("graph query cancelled"));
            }
            let mut next = Frontier::new(request.max_visited - seen.len());
            for entity in frontier {
                if cancel() {
                    return Err(invalid("graph query cancelled"));
                }
                if matches!(request.direction, Direction::Outgoing | Direction::Both) {
                    self.bfs_direction(
                        h,
                        entity,
                        Direction::Outgoing,
                        &request,
                        &mut next,
                        &seen,
                        &mut scanned_edges,
                        &mut cancel,
                    )?;
                }
                if matches!(request.direction, Direction::Incoming | Direction::Both) {
                    self.bfs_direction(
                        h,
                        entity,
                        Direction::Incoming,
                        &request,
                        &mut next,
                        &seen,
                        &mut scanned_edges,
                        &mut cancel,
                    )?;
                }
            }
            let next = next.into_sorted();
            if seen.len() + next.len() > request.max_visited {
                return Err(invalid("BFS visited limit exceeded"));
            }
            merge_sorted_disjoint(&mut seen, &next, &mut merged);
            if depth >= request.min_depth {
                if nodes.len() + next.len() > request.result_limit {
                    return Err(invalid("BFS result limit exceeded"));
                }
                nodes.extend(
                    next.iter()
                        .copied()
                        .map(|entity| TraversalNode { entity, depth }),
                );
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
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

    fn preflight_incident_edges(&self, entity: EntityId) -> Result<Vec<EdgeKey>> {
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
                let edge = parse_edge_key(&key, tag)?;
                self.validate_stored_edge_ids(h, edge)?;
                if tag == PRIMARY_EDGE {
                    decode_properties(&value)?;
                    if self.store()?.get(&edge_key(REVERSE_EDGE, edge))?.as_deref() != Some(&[]) {
                        return Err(corrupt("missing/nonempty reverse edge marker"));
                    }
                } else {
                    if !value.is_empty() {
                        return Err(corrupt("nonempty reverse edge marker"));
                    }
                    let primary = self
                        .store()?
                        .get(&edge_key(PRIMARY_EDGE, edge))?
                        .ok_or_else(|| corrupt("reverse edge without authoritative primary"))?;
                    decode_properties(&primary)?;
                }
                edges.insert(edge);
                if edges.len() > MAX_CASCADE {
                    return Err(invalid("entity has more than 256 incident graph edges"));
                }
            }
        }
        Ok(edges.into_iter().collect())
    }

    /// Parent `Database::delete` calls this before any entity/index mutation.
    /// The preflight reads at most 257 distinct incident tuples. Only after it
    /// succeeds are complete primary/reverse pairs removed. A checksum-valid
    /// omitted reverse marker cannot prove that no incoming primary exists;
    /// missing authoritative properties are never reconstructed from markers.
    pub(super) fn cascade_graph_delete(&mut self, entity: EntityId) -> Result<()> {
        let edges = self.preflight_incident_edges(entity)?;
        let result = (|| {
            for edge in edges {
                if !self.writer()?.delete(&edge_key(PRIMARY_EDGE, edge))?
                    || !self.writer()?.delete(&edge_key(REVERSE_EDGE, edge))?
                {
                    return Err(corrupt("incident edge disappeared during cascade"));
                }
            }
            Ok(())
        })();
        self.finish(result)
    }
}

#[cfg(test)]
#[path = "graph_fault_tests.rs"]
mod fault_tests;
