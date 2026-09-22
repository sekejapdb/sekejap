//! The Vamana/DiskANN graph: a disk-first approximate vector index.
//!
//! WHY A GRAPH AND NOT A SCAN. The `quantized` family beside this one is a
//! bounded LINEAR scan over int8 codes: it reads every entry of the index, so
//! at 50 million rows and 4096 lanes one query touches about 205 GB. This
//! family reads a few hundred records instead. It is a SINGLE-layer graph
//! with deliberately long-range edges, which is the shape that survives
//! living on disk; HNSW's tower of layers assumes the graph is resident in
//! RAM and is the wrong premise for this engine.
//!
//! WHAT IS ON DISK. TWO records per node, in TWO keyspaces, both behind the
//! additive feature bit `0x8000`:
//!
//! ```text
//! key   = 0x7D || ordered(index id) || ordered(sequence)
//! value, sequence == 0 -- the GRAPH HEADER of that index:
//!         version:u8 | degree:u8 | build list:u16be | alpha*100:u16be
//!       | entry:u64be | nodes:u64be | entry chosen at:u64be
//! value, sequence > 0 -- one node's HEAD, and nothing else:
//!         locator:6 | scale:f64le | codes:i8[dim]
//!
//! key   = 0x7F || ordered(index id) || ordered(sequence)
//! value -- that node's ADJACENCY:
//!         own:u16be | back:u16be
//!       | (neighbour:u64be, distance:f32be) * (own + back)
//! ```
//!
//! The HEAD is byte-for-byte what [`super::quantized::encode_entry`] writes:
//! the same locator into the immutable `0x60` f32 sidecar and the same
//! symmetric int8 codes. So the head is DERIVED from the row and is checked
//! against it -- at rerank, in `verify_indexed_source` and at build -- exactly
//! as the quantized family's entry is (Law 5). The adjacency is not derived
//! from any one row: it is the index's own structure, and what verification
//! checks about it is the structural invariant below.
//!
//! WHY THE TWO ARE APART, which is the whole reason this file has two
//! keyspaces rather than one. The head's length is the DIMENSION: at 4,096
//! lanes it is 4,110 bytes, larger than a 4,096-byte page, so a node whose
//! head and adjacency shared one record lived in an overflow chain. Every
//! back edge an insert appends is a read-modify-write of a NEIGHBOUR, and
//! rewriting a shared record rewrote the codes and the whole chain with them
//! -- `R = 48` neighbours times the chain, per insert. Measured at 4,096
//! lanes over a 600-node graph, one insert appended 2,780,624 bytes of WAL
//! (2.65 MiB), so a build of any batch size was refused by the page-WAL's
//! 16 MiB managed-byte allowance and halving the batch could not help: the
//! cost was never per row. Split, an edge append rewrites ONE adjacency
//! record -- at most `4 + 2R * 12 = 1,156` bytes, one page, never a chain --
//! and the codes are not touched at all. The head is written ONCE, when the
//! node is linked, and is immutable for the node's life: an update that moved
//! the vector is an unlink followed by a link, not a rewrite.
//!
//! The price, stated: a node the walk reaches costs TWO point reads instead
//! of one. The bytes read go DOWN (a 4,110-byte head plus a 1,156-byte
//! adjacency against a 5,270-byte chained record), the seeks go up by one per
//! node, and both keyspaces are keyed by the same `(index, sequence)` so the
//! two reads descend the same shape of tree.
//!
//! THE STRUCTURAL INVARIANT, one sentence: the neighbour relation is
//! SYMMETRIC and closed. If `a` names `b` then `b` names `a`, no list names a
//! sequence with no node record, no list names its own node, no list holds a
//! duplicate, and no list is longer than [`DEGREE`]. Symmetry is what makes a
//! delete BOUNDED -- the nodes that point AT a node are exactly the nodes it
//! points at, so unlinking reads `R` records instead of the whole keyspace --
//! and it is what makes "a delete strands no neighbour list" checkable.
//!
//! MAINTENANCE. Live, on the write path, like every other family here: see
//! [`maintain_node`]. There is no separate build algorithm -- the late build
//! is the insert path replayed over the rows that already exist -- so a
//! graph this engine answers from is never a graph some earlier state of the
//! data built. Costs are stated on [`link_node`] and [`unlink_node`].
use crate::collections::{
    catalog, corrupt, invalid, layout_id, ordered, read_ordered, vector_key, CollectionId,
    Database, EntityId, Error, IndexFamily, IndexId, IndexInfo, IndexState, Result, VectorCells,
};
use crate::index::vector::exact::{VectorHit, VectorMetric};
use crate::index::vector::quantized::{ApproxVectorMethod, ApproxVectorResult};
use crate::vector_quant::{self, Metric as QuantMetric};
use crate::{Kind, Layout};
use std::collections::{BinaryHeap, HashMap, HashSet};

/// The additive logical feature bit. A file that declares it is refused WHOLE
/// by any binary whose `SUPPORTED_LOGICAL_FEATURES` predates it, as
/// `Unsupported` and with no byte of the source changed (Law 8).
pub const VAMANA_FEATURE: u64 = 0x8000;
/// The node keyspace: the graph header at sequence 0, and one node HEAD per
/// sequence after it. `docs/core/FORMAT_V2.md` records it, and a new keyspace
/// plus an additive bit is the only change format v2 permits.
pub const VAMANA_ENTRY: u8 = 0x7D;
/// The ADJACENCY keyspace, under the same feature bit.
///
/// A second tag rather than a tail on the node record, because the head's
/// length is the dimension and the adjacency's is the degree: sharing one
/// record made every edge append rewrite the codes, and past a page it
/// rewrote an overflow chain (see the module comment). `0x7F` was the last
/// free tag in the index run; `docs/core/FORMAT_V2.md`'s Extension boundary
/// records it as taken.
pub const VAMANA_ADJACENCY: u8 = 0x7F;
/// Node-record and header encoding version. Frozen.
pub const GRAPH_VERSION: u8 = 1;
pub(crate) const OPTIONS: u8 = 0;

/// R, the degree one node's list is pruned back to.
///
/// 48, the middle of the DiskANN paper's 32-to-64 range, chosen by measuring
/// both ends. Over 10,000 clustered 128-lane rows, recall@10 rose from 0.67
/// to 0.84 at a search list of 40 and from 0.88 to 0.92 at 100 when R went
/// from 32 to 48, for 872 bytes per row instead of 641 and about half again
/// as many records read per query. 64 was not measured to pay for the page
/// density it costs: an edge is 12 bytes, so at R=48 a neighbour list is
/// about 580 bytes on its own and seven of them share a 4 KiB page of the
/// `0x7F` keyspace.
///
/// It is also the unit every cost in this file is stated in: an insert is
/// O(L + R) reads and O(R) writes, a delete O(R) of each.
pub const DEGREE: usize = 48;
/// The HARD cap on a stored neighbour list, and the slack that makes an
/// insert cheap.
///
/// A back edge is APPENDED, never traded against the list it joins: trading
/// is what destroyed the graph in the first measurement, because a new node's
/// chosen neighbours are the popular ones, their lists are already full, and
/// a trade decided by distance alone threw the newcomer's edge away every
/// time -- so late rows arrived nearly unlinked and recall fell with the
/// corpus. A list is therefore allowed to run up to this ceiling and is
/// RobustPruned back to [`DEGREE`] when it reaches it, which is the only
/// rule that keeps alpha's long edges (see [`prune_neighbour`]).
///
/// The slack is what makes that affordable: a prune reads the candidates'
/// codes, so doing one per back edge would cost `R` reads per back edge and
/// `R^2` per insert. At a ceiling of `2R` a node is pruned once per `R` back
/// edges instead, which is about two extra reads per back edge amortised.
/// The price is on disk: a list may hold 64 edges, so 768 bytes rather than
/// 384, and the measured bytes per row in `docs/core/VECTOR_CAPACITY.md` are
/// the real occupancy between the two.
pub const MAX_DEGREE: usize = DEGREE * 2;
/// L, the search list the BUILD uses -- how wide the greedy search that finds
/// a new node's neighbours is allowed to get. 100 is the paper's figure and
/// the one the recall floor was measured at. The QUERY's list is the caller's
/// `ef` (`diskann.query_search_list_size` / `ef_search`) and is not this.
pub const BUILD_SEARCH_LIST: usize = 100;
/// Alpha * 100. 1.2 is the paper's value: an edge `p -> q` survives pruning
/// only when no already-kept neighbour `p*` satisfies
/// `alpha * d(p*, q) <= d(p, q)`, so raising alpha above 1 is what KEEPS the
/// long-range edges a single-layer graph navigates by.
pub const ALPHA_HUNDREDTHS: u16 = 120;
/// How many of the nodes one insert's search REACHED are offered to the
/// prune. Vamana prunes over the visited set, not over the final list: the
/// nodes the walk passed through on its way in are where its long-range
/// edges come from. Twice the search list, which bounds the transient memory
/// of an insert at `2 * L * (14 + dim)` bytes.
const BUILD_VISITED: usize = BUILD_SEARCH_LIST * 2;
/// Bytes per stored edge: neighbour sequence and the approximate distance to
/// it. The distance is what makes an overflowing neighbour list trimmable
/// without reading the neighbours it holds, which is what keeps an insert at
/// O(R) reads instead of O(R^2).
const EDGE: usize = 12;
const HEADER_BYTES: usize = 30;
/// How many nodes the entry-point refresh samples. 64 seeks and 64*64 int8
/// distances, paid once per DOUBLING of the graph, is under two reads per
/// insert amortised.
const SAMPLE: usize = 64;
/// The smallest graph that gets a chosen entry point at all. Below it the
/// first node inserted is as good as any.
const SAMPLE_FLOOR: u64 = 64;

/// One stored edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Edge {
    pub seq: u64,
    /// The approximate (int8) distance between the two endpoints under the
    /// NAVIGATION metric. A cache for trimming, never an answer: every
    /// distance a caller sees is the exact f32 rerank's.
    pub distance: f32,
}

/// A node's neighbour list, split by where the edge came from.
///
/// `own` are the edges this node's OWN robust prune chose when it was
/// inserted -- the alpha-pruned, deliberately long-range ones. `back` are the
/// edges other nodes' inserts added into this list. When the list is full the
/// trim takes from `back` first, so a node never loses the long edges that
/// make it navigable to pay for a short one a later insert wanted.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Adjacency {
    pub own: Vec<Edge>,
    pub back: Vec<Edge>,
}

impl Adjacency {
    pub(crate) fn len(&self) -> usize {
        self.own.len() + self.back.len()
    }
    pub(crate) fn sequences(&self) -> impl Iterator<Item = u64> + '_ {
        self.own.iter().chain(self.back.iter()).map(|edge| edge.seq)
    }
    fn holds(&self, seq: u64) -> bool {
        self.sequences().any(|other| other == seq)
    }
    fn remove(&mut self, seq: u64) -> bool {
        let before = self.len();
        self.own.retain(|edge| edge.seq != seq);
        self.back.retain(|edge| edge.seq != seq);
        before != self.len()
    }
    /// Append one edge. Nothing is traded away here: overflow past
    /// [`MAX_DEGREE`] is settled by [`prune_neighbour`], which is the only
    /// place an edge is dropped for want of room.
    fn append(&mut self, edge: Edge) {
        self.back.push(edge);
    }
    fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.own.iter().chain(self.back.iter()).copied()
    }
}

/// The per-index graph header, at sequence 0 of the index's `0x7D` range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GraphHeader {
    /// The node every search starts from. 0 means the graph is empty.
    pub entry: u64,
    pub nodes: u64,
    /// The node count when `entry` was last chosen. The refresh is due when
    /// the graph has doubled since.
    pub entry_at: u64,
}

pub(crate) fn node_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![VAMANA_ENTRY];
    key.extend(ordered(id.0));
    key
}

pub(crate) fn node_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = node_prefix(id);
    key.extend(ordered(sequence));
    key
}

pub(crate) fn adjacency_prefix(id: IndexId) -> Vec<u8> {
    let mut key = vec![VAMANA_ADJACENCY];
    key.extend(ordered(id.0));
    key
}

/// One node's adjacency record. Keyed by the same `(index, sequence)` the
/// node's head is, so the two reads descend the same shape of tree and a
/// walk that has one sequence has both keys without another lookup.
pub(crate) fn adjacency_key(id: IndexId, sequence: u64) -> Vec<u8> {
    let mut key = adjacency_prefix(id);
    key.extend(ordered(sequence));
    key
}

/// The header record's key: sequence 0, which no entity can have, so the
/// header sorts first inside the index's own range and shares its lifetime.
pub(crate) fn header_key(id: IndexId) -> Vec<u8> {
    node_key(id, 0)
}

pub(crate) fn dimension(index: &IndexInfo) -> Result<usize> {
    match (&index.family, &index.kind) {
        (IndexFamily::VamanaGraph, Kind::Vector(dimension))
            if (1..=vector_quant::MAX_DIMENSION).contains(dimension) =>
        {
            Ok(*dimension)
        }
        _ => Err(corrupt("vamana graph descriptor family/kind")),
    }
}

/// The head length: locator, quantizer scale and one code per lane.
pub(crate) fn head_len(dimension: usize) -> usize {
    6 + 8 + dimension
}

/// The metric the GRAPH is built under.
///
/// One metric, always, because the metric is a property of the QUERY here and
/// not of the descriptor: `ORDER BY col <=> $1` and `col <-> $1` reach the
/// same index. A proximity graph is a navigation structure rather than an
/// answer, so it is built once under squared L2 over the int8 codes and
/// SEARCHED under whichever metric the query named -- the codes are in the
/// record, so any metric can be scored at query time -- and the answer's
/// order is the exact f32 rerank's under the query's own metric. The cost is
/// stated in `docs/lang/INDEX_CONTRACT.md`: recall under negative inner
/// product is lower than under cosine and L2, because an L2 neighbourhood is
/// not an inner-product neighbourhood.
const NAVIGATION: QuantMetric = QuantMetric::SquaredL2;

/// The navigation distance between one node's codes and a widened set of
/// lanes: TRUE L2, not the squared form the kernel computes.
///
/// The square root matters and is not cosmetic. RobustPrune's test is
/// `alpha * d(kept, q) <= d(target, q)` on a METRIC, and alpha is what keeps
/// long edges. Applied to SQUARED distances the same constant acts as
/// `sqrt(alpha)` -- 1.095 instead of 1.2 -- so the prune covers more
/// candidates, keeps the near ones and throws away exactly the long-range
/// edges a single-layer graph navigates by. Measured over 10,000 clustered
/// 128-lane rows, recall@10 at a search list of 100 was 0.81 on the squared
/// form and 0.97 on this one.
fn navigation(scale: f64, codes: &[u8], lanes: &[f64], norm: f64) -> Option<f64> {
    vector_quant::score_i8_hot(scale, codes, lanes, norm, NAVIGATION).map(f64::sqrt)
}

fn metric(metric: VectorMetric) -> QuantMetric {
    match metric {
        VectorMetric::Cosine => QuantMetric::Cosine,
        VectorMetric::SquaredL2 => QuantMetric::SquaredL2,
        VectorMetric::NegativeDot => QuantMetric::NegativeDot,
    }
}

pub(crate) fn encode_header(header: GraphHeader) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER_BYTES);
    b.push(GRAPH_VERSION);
    b.push(DEGREE as u8);
    b.extend((BUILD_SEARCH_LIST as u16).to_be_bytes());
    b.extend(ALPHA_HUNDREDTHS.to_be_bytes());
    b.extend(header.entry.to_be_bytes());
    b.extend(header.nodes.to_be_bytes());
    b.extend(header.entry_at.to_be_bytes());
    b
}

pub(crate) fn decode_header(b: &[u8]) -> Result<GraphHeader> {
    if b.len() != HEADER_BYTES {
        return Err(corrupt("vamana graph header length"));
    }
    if b[0] != GRAPH_VERSION {
        return Err(Error::Unsupported("vamana graph header version".into()));
    }
    // The build parameters are part of the shape of the graph on disk, not a
    // knob a later build may reinterpret: a binary whose R or alpha differ
    // would maintain a graph it did not build. Refused as Unsupported, with
    // the bytes intact (Law 8).
    if usize::from(b[1]) != DEGREE
        || u16::from_be_bytes(b[2..4].try_into().unwrap()) != BUILD_SEARCH_LIST as u16
        || u16::from_be_bytes(b[4..6].try_into().unwrap()) != ALPHA_HUNDREDTHS
    {
        return Err(Error::Unsupported(
            "vamana graph degree/search list/alpha".into(),
        ));
    }
    let entry = u64::from_be_bytes(b[6..14].try_into().unwrap());
    let nodes = u64::from_be_bytes(b[14..22].try_into().unwrap());
    let entry_at = u64::from_be_bytes(b[22..30].try_into().unwrap());
    if (entry == 0) != (nodes == 0) || entry_at > nodes {
        return Err(corrupt("vamana graph header entry/count"));
    }
    Ok(GraphHeader {
        entry,
        nodes,
        entry_at,
    })
}

/// One node's adjacency record: at most `4 + 2R * EDGE` bytes, which is
/// 1,156 at `R = 48` and therefore always inside one page. This is the ONLY
/// record an edge append or a reprune rewrites.
fn encode_adjacency(adjacency: &Adjacency) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + adjacency.len() * EDGE);
    b.extend((adjacency.own.len() as u16).to_be_bytes());
    b.extend((adjacency.back.len() as u16).to_be_bytes());
    for edge in adjacency.own.iter().chain(adjacency.back.iter()) {
        b.extend(edge.seq.to_be_bytes());
        b.extend(edge.distance.to_be_bytes());
    }
    b
}

/// Check one node record and hand back the head it is.
///
/// The value IS the head now, so this checks exactly what the quantized
/// family checks of its own entry -- the locator decodes, the codes decode,
/// and there is not one trailing byte -- and nothing else. What no single
/// record can see is checked elsewhere: the head against the row it derives
/// from at rerank and in `verify_indexed_source`, the adjacency by
/// [`decode_adjacency`], and the closure by verification.
pub(crate) fn decode_head(value: &[u8], dimension: usize) -> Result<&[u8]> {
    let head = head_len(dimension);
    if value.len() != head {
        return Err(corrupt("vamana node record length"));
    }
    crate::index::vector::exact::decode_locator(&value[..6])?;
    vector_quant::decode(&value[6..head], dimension)
        .map_err(|_| corrupt("vamana node record codec"))?;
    Ok(value)
}

/// Decode one node's adjacency record.
///
/// Every structural rule a single record can carry is checked HERE, so a
/// walk that reads one never has to trust it: degrees inside [`MAX_DEGREE`],
/// no self-loop, no duplicate, no zero sequence, no trailing bytes. What one
/// record cannot see -- that the neighbour exists and names this node back --
/// is the closure `verify_indexed_source` checks.
pub(crate) fn decode_adjacency(value: &[u8], own: u64) -> Result<Adjacency> {
    if value.len() < 4 {
        return Err(corrupt("vamana adjacency record length"));
    }
    let own_degree = usize::from(u16::from_be_bytes(value[..2].try_into().unwrap()));
    let back_degree = usize::from(u16::from_be_bytes(value[2..4].try_into().unwrap()));
    let degree = own_degree + back_degree;
    if degree > MAX_DEGREE || value.len() != 4 + degree * EDGE {
        return Err(corrupt("vamana node degree"));
    }
    let mut adjacency = Adjacency {
        own: Vec::with_capacity(own_degree),
        back: Vec::with_capacity(back_degree),
    };
    let mut at = 4;
    for position in 0..degree {
        let seq = u64::from_be_bytes(value[at..at + 8].try_into().unwrap());
        let distance = f32::from_be_bytes(value[at + 8..at + 12].try_into().unwrap());
        at += EDGE;
        if seq == 0 || seq == own {
            return Err(corrupt("vamana node neighbour identity"));
        }
        let edge = Edge { seq, distance };
        if position < own_degree {
            adjacency.own.push(edge);
        } else {
            adjacency.back.push(edge);
        }
    }
    let mut seen = HashSet::with_capacity(degree);
    for seq in adjacency.sequences() {
        if !seen.insert(seq) {
            return Err(corrupt("vamana node duplicate neighbour"));
        }
    }
    Ok(adjacency)
}

/// The codes of one head, as the hot int8 kernel wants them.
fn head_codes(head: &[u8], dimension: usize) -> (f64, &[u8]) {
    (
        f64::from_le_bytes(head[6..14].try_into().unwrap()),
        &head[14..14 + dimension],
    )
}

/// Widen one node's int8 codes into the f64 lanes the kernel scores against,
/// so that node can play the part of a query. `(lanes, sum of squares)`.
fn widen_codes(scale: f64, codes: &[u8]) -> (Vec<f64>, f64) {
    let mut lanes = Vec::with_capacity(codes.len());
    let mut norm = 0.0f64;
    for code in codes {
        let lane = scale * f64::from(*code as i8);
        norm += lane * lane;
        lanes.push(lane);
    }
    (lanes, norm)
}

/// One node's head, ready to be scored and pruned against.
#[derive(Clone)]
struct Node {
    seq: u64,
    distance: f64,
    head: Vec<u8>,
}

/// The head of one row, derived exactly as the quantized family derives its
/// entry, so the two families agree byte for byte about what a row's code is.
fn desired_head(
    index: &IndexInfo,
    layout: &Layout,
    vectors: &VectorCells,
) -> Result<Option<Vec<u8>>> {
    let expected = dimension(index)?;
    let Some((ordinal, (_, kind))) = layout
        .fields
        .iter()
        .enumerate()
        .find(|(_, (name, _))| name == &index.field)
    else {
        return Err(corrupt("current indexed vector field is absent"));
    };
    if kind != &Kind::Vector(expected) {
        return Err(corrupt("current indexed vector field changed kind"));
    }
    let Some((_, raw)) = vectors.iter().find(|(field, _)| *field == ordinal) else {
        return Ok(None);
    };
    let layout_id = u32::try_from(layout.id).map_err(corrupt)?;
    let locator = crate::index::vector::exact::encode_locator(layout_id, ordinal)?;
    crate::index::vector::quantized::encode_entry(locator, raw, expected).map(Some)
}

/// The same head, derived from an immutable row and its authoritative
/// sidecar: the late build's side of [`desired_head`].
pub(crate) fn build_head(
    db: &Database,
    index: &IndexInfo,
    id: EntityId,
    row: &[u8],
) -> Result<Option<Vec<u8>>> {
    let expected = dimension(index)?;
    let layout_id = layout_id(row)?;
    let layout = db.layout(layout_id)?;
    let ordinal = crate::dense_v3::locate_vector(&layout, row, &index.field, expected)
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("historical vector field") {
                invalid(message)
            } else {
                corrupt(message)
            }
        })?;
    let Some(ordinal) = ordinal else {
        return Ok(None);
    };
    let raw = db
        .store()?
        .get(&vector_key(id, ordinal))?
        .ok_or_else(|| corrupt("indexed vector sidecar is missing"))?;
    let locator = crate::index::vector::exact::encode_locator(layout_id, ordinal)?;
    crate::index::vector::quantized::encode_entry(locator, &raw, expected).map(Some)
}

pub(crate) fn validate_locator(
    db: &Database,
    index: &IndexInfo,
    locator: &[u8; 6],
) -> Result<usize> {
    let expected = dimension(index)?;
    let (layout_id, ordinal) = crate::index::vector::exact::decode_locator(locator)?;
    let layout = db.layout(layout_id)?;
    if layout.id != u64::from(layout_id)
        || !matches!(
            layout.fields.get(ordinal),
            Some((name, Kind::Vector(found))) if name == &index.field && *found == expected
        )
    {
        return Err(corrupt("vamana node locator field/layout mismatch"));
    }
    Ok(ordinal)
}

// ── The records, read and written one at a time ───────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub head: Vec<u8>,
    pub adjacency: Adjacency,
}

pub(crate) fn read_header(db: &Database, id: IndexId) -> Result<GraphHeader> {
    match db.store()?.get(&header_key(id))? {
        Some(bytes) => decode_header(&bytes),
        None => Ok(GraphHeader {
            entry: 0,
            nodes: 0,
            entry_at: 0,
        }),
    }
}

fn put_header(db: &mut Database, id: IndexId, header: GraphHeader) -> Result<()> {
    let value = encode_header(header);
    db.writer()?.put(&header_key(id), &value)?;
    Ok(())
}

/// One node's head, without its adjacency: what a rerank and a verification
/// want, and one point read rather than two.
fn read_head(
    db: &Database,
    id: IndexId,
    dimension: usize,
    seq: u64,
) -> Result<Option<Vec<u8>>> {
    let Some(bytes) = db.store()?.get(&node_key(id, seq))? else {
        return Ok(None);
    };
    decode_head(&bytes, dimension)?;
    Ok(Some(bytes))
}

/// One node's adjacency. A node whose head exists always has one, even when
/// it is empty: the two keyspaces hold exactly the same sequences, which is
/// what lets verification say a missing one is damage rather than a shape.
fn read_adjacency(db: &Database, id: IndexId, seq: u64) -> Result<Option<Adjacency>> {
    let Some(bytes) = db.store()?.get(&adjacency_key(id, seq))? else {
        return Ok(None);
    };
    decode_adjacency(&bytes, seq).map(Some)
}

pub(crate) fn read_record(
    db: &Database,
    id: IndexId,
    dimension: usize,
    seq: u64,
) -> Result<Option<Record>> {
    let Some(head) = read_head(db, id, dimension, seq)? else {
        return Ok(None);
    };
    let adjacency = read_adjacency(db, id, seq)?
        .ok_or_else(|| corrupt("vamana node has no adjacency record"))?;
    Ok(Some(Record { head, adjacency }))
}

/// Write the node's HEAD. Called once, when the node is linked: the head is
/// immutable for the node's life, and keeping it out of the edge path is the
/// whole point of the split.
fn put_head(db: &mut Database, id: IndexId, seq: u64, head: &[u8]) -> Result<()> {
    db.writer()?.put(&node_key(id, seq), head)?;
    Ok(())
}

/// Write one node's ADJACENCY. At most 1,156 bytes, whatever the dimension.
fn put_adjacency(db: &mut Database, id: IndexId, seq: u64, adjacency: &Adjacency) -> Result<()> {
    let value = encode_adjacency(adjacency);
    db.writer()?.put(&adjacency_key(id, seq), &value)?;
    Ok(())
}

/// The records one write is holding, and whether each one has been changed.
///
/// A prune READS the codes of every candidate it weighs and WRITES only the
/// endpoints whose edge it dropped, so the two are counted apart: the clean
/// ones cost a read and nothing else.
#[derive(Default)]
struct Pending {
    held: HashMap<u64, Record>,
    dirty: HashSet<u64>,
}

impl Pending {
    fn get(&self, seq: u64) -> Option<&Record> {
        self.held.get(&seq)
    }
    fn touch(&mut self, seq: u64) -> Option<&mut Record> {
        self.dirty.insert(seq);
        self.held.get_mut(&seq)
    }
    /// Write back what changed, which is ADJACENCY and never a head. A head
    /// is written once by [`link_node`] and never rewritten, so a flush of
    /// `R` dirty neighbours costs `R` records of at most 1,156 bytes each
    /// whatever the dimension is.
    fn flush(&self, db: &mut Database, id: IndexId) -> Result<()> {
        for seq in &self.dirty {
            if let Some(record) = self.held.get(seq) {
                put_adjacency(db, id, *seq, &record.adjacency)?;
            }
        }
        Ok(())
    }
}

/// Read one node into the pending set if it is not already there.
fn fetch(
    db: &Database,
    pending: &mut Pending,
    id: IndexId,
    dimension: usize,
    seq: u64,
) -> Result<bool> {
    if pending.held.contains_key(&seq) {
        return Ok(true);
    }
    match read_record(db, id, dimension, seq)? {
        Some(record) => {
            pending.held.insert(seq, record);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// RobustPrune one node's overflowing list back to [`DEGREE`].
///
/// This is Vamana's own rule and not a nearest-R trim, and the difference is
/// the whole point: a nearest-R trim drops the edges that reach FURTHEST,
/// which are exactly the edges a single-layer graph navigates by. Alpha keeps
/// an edge `n -> q` unless some closer kept neighbour `p` already covers that
/// direction, `alpha * d(p, q) <= d(n, q)`.
///
/// Every edge it drops is dropped on BOTH sides -- the far endpoint is one of
/// the candidates, so its record is already in hand and no read is added for
/// the symmetry.
///
/// COST: one read per candidate (at most `MAX_DEGREE`, once, cached in
/// `pending`) and one ADJACENCY write per endpoint whose edge was dropped.
/// No head is written here, at any dimension.
fn prune_neighbour(
    db: &Database,
    index: &IndexInfo,
    dimension: usize,
    pending: &mut Pending,
    seq: u64,
) -> Result<()> {
    let Some(record) = pending.get(seq) else {
        return Ok(());
    };
    if record.adjacency.len() <= MAX_DEGREE {
        return Ok(());
    }
    let edges: Vec<Edge> = record.adjacency.edges().collect();
    let mut candidates = Vec::with_capacity(edges.len());
    for edge in &edges {
        if !fetch(db, pending, index.id, dimension, edge.seq)? {
            return Err(corrupt("vamana graph neighbour record is missing"));
        }
        candidates.push(Node {
            seq: edge.seq,
            distance: f64::from(edge.distance),
            head: pending.get(edge.seq).expect("fetched above").head.clone(),
        });
    }
    candidates.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.seq.cmp(&right.seq))
    });
    let kept = robust_prune(candidates, dimension, alpha(), DEGREE);
    let keep: HashSet<u64> = kept.iter().map(|edge| edge.seq).collect();
    for edge in &edges {
        if keep.contains(&edge.seq) {
            continue;
        }
        if let Some(other) = pending.touch(edge.seq) {
            other.adjacency.remove(seq);
        }
    }
    let record = pending.touch(seq).expect("held above");
    record.adjacency = Adjacency {
        own: kept,
        back: Vec::new(),
    };
    Ok(())
}

// ── Greedy best-first search ──────────────────────────────────────────────

/// One node the search has reached.
struct Reached {
    node: Node,
    adjacency: Adjacency,
    expanded: bool,
    /// False when the metric refuses this stored vector -- a zero vector
    /// under cosine. Such a node is still a place to walk THROUGH; it is
    /// never an answer.
    scored: bool,
}

/// Greedy best-first over the graph, bounded by `list`.
///
/// `keep_visited` is how many of the nodes the walk REACHED to hand back
/// beyond the final list. The build prunes over the visited set rather than
/// over the list -- that is what Vamana's construction does, and a candidate
/// the walk passed through on its way in is exactly the kind of long edge
/// alpha exists to keep. Zero for a query, which wants the list and nothing
/// else.
///
/// This is the whole read path. From the entry point the walk repeatedly
/// expands the closest candidate it has not expanded yet, scoring that node's
/// neighbours against the query from the int8 codes the neighbour's own
/// record carries. It stops when the `list`-bounded candidate set holds no
/// unexpanded node, which is the classic termination: a node enters the set
/// only by beating its worst member, and the set never grows past `list`.
///
/// COST, and the reason this family exists: the records READ are the nodes
/// reached, not the corpus. A node is read at most once -- `seen` is checked
/// before the read, not after -- so the walk is bounded by the number of
/// distinct nodes within `list` hops of the query, which at `list = 40` over
/// a 20,000-node graph is on the order of 500 nodes where the linear family
/// reads 20,000. A node costs TWO point reads since the adjacency moved into
/// its own keyspace -- its head and its list -- and fewer bytes than the one
/// chained record it replaced.
#[allow(clippy::too_many_arguments)]
fn greedy_search(
    db: &Database,
    index: &IndexInfo,
    dimension: usize,
    query: &[f64],
    query_norm: f64,
    metric: QuantMetric,
    entry: u64,
    list: usize,
    exclude: u64,
    max_examined: usize,
    keep_visited: usize,
    progress: crate::index::vector::exact::ScanProgress<'_>,
) -> Result<(Vec<Node>, Vec<Node>, usize)> {
    let mut visited: Vec<Node> = Vec::new();
    let mut pool: Vec<Reached> = Vec::with_capacity(list + 1);
    let mut seen: HashSet<u64> = HashSet::new();
    let mut examined = 0usize;
    let mut pending = 0u64;
    let mut reach = |db: &Database,
                     pool: &mut Vec<Reached>,
                     seen: &mut HashSet<u64>,
                     examined: &mut usize,
                     pending: &mut u64,
                     seq: u64|
     -> Result<()> {
        if seq == exclude || !seen.insert(seq) {
            return Ok(());
        }
        if *examined == max_examined {
            return Err(Error::Kernel(kernel::Error::ResourceLimit(
                "vamana graph max_examined exceeded",
            )));
        }
        *examined += 1;
        *pending += 1;
        if *pending == crate::index::vector::exact::SCAN_STEP {
            progress(crate::index::vector::exact::ScanStep::Scored(
                std::mem::take(pending),
            ))?;
        }
        let Some(record) = read_record(db, index.id, dimension, seq)? else {
            // The closure invariant says every named neighbour has a record.
            // A missing one is damage the caller must hear about, not a node
            // to step over silently (Law 5).
            return Err(corrupt("vamana graph neighbour record is missing"));
        };
        let (scale, codes) = head_codes(&record.head, dimension);
        // Under the navigation metric the walk works in TRUE L2, so the
        // distances it hands the prune are the ones alpha is defined on.
        let scored = if metric == NAVIGATION && keep_visited > 0 {
            navigation(scale, codes, query, query_norm)
        } else {
            vector_quant::score_i8_hot(scale, codes, query, query_norm, metric)
        };
        let node = Node {
            seq,
            distance: scored.unwrap_or(f64::INFINITY),
            head: record.head,
        };
        if scored.is_some() && visited.len() < keep_visited {
            visited.push(node.clone());
        }
        let at = pool
            .partition_point(|other| (other.node.distance, other.node.seq) < (node.distance, seq));
        pool.insert(
            at,
            Reached {
                node,
                adjacency: record.adjacency,
                expanded: false,
                scored: scored.is_some(),
            },
        );
        pool.truncate(list);
        Ok(())
    };
    reach(db, &mut pool, &mut seen, &mut examined, &mut pending, entry)?;
    loop {
        let Some(at) = pool.iter().position(|reached| !reached.expanded) else {
            break;
        };
        pool[at].expanded = true;
        let neighbours: Vec<u64> = pool[at].adjacency.sequences().collect();
        for seq in neighbours {
            reach(db, &mut pool, &mut seen, &mut examined, &mut pending, seq)?;
        }
    }
    progress(crate::index::vector::exact::ScanStep::Scored(pending))?;
    visited.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.seq.cmp(&right.seq))
    });
    Ok((
        pool.into_iter()
            .filter(|reached| reached.scored)
            .map(|reached| reached.node)
            .collect(),
        visited,
        examined,
    ))
}

// ── Robust pruning ────────────────────────────────────────────────────────

/// Vamana's RobustPrune, with `alpha`.
///
/// `candidates` are the nodes the greedy search reached, ascending by their
/// distance to the node being linked. The closest survivor is kept, and every
/// remaining candidate `q` for which `alpha * d(kept, q) <= d(target, q)` is
/// DROPPED -- kept's edge already covers that direction. Raising alpha above
/// 1 makes that covering test stricter, so edges that reach much further than
/// the nearest neighbours survive, and those long edges are the whole reason
/// a single-layer graph can be navigated in a few hops.
///
/// COST: at most `DEGREE` widenings and `DEGREE * candidates` int8 distances,
/// all in memory -- the candidates' codes were read by the search that
/// produced them, so pruning reads nothing.
fn robust_prune(
    candidates: Vec<Node>,
    dimension: usize,
    alpha: f64,
    degree: usize,
) -> Vec<Edge> {
    let mut remaining = candidates;
    let mut kept: Vec<Edge> = Vec::with_capacity(degree);
    while !remaining.is_empty() && kept.len() < degree {
        let best = remaining.remove(0);
        kept.push(Edge {
            seq: best.seq,
            distance: best.distance as f32,
        });
        if kept.len() == degree {
            break;
        }
        let (scale, codes) = head_codes(&best.head, dimension);
        let (lanes, norm) = widen_codes(scale, codes);
        remaining.retain(|candidate| {
            let (other_scale, other_codes) = head_codes(&candidate.head, dimension);
            match navigation(other_scale, other_codes, &lanes, norm) {
                Some(covered) => alpha * covered > candidate.distance,
                None => true,
            }
        });
    }
    kept
}

// ── Maintenance: link, unlink, and what a write does ──────────────────────

fn alpha() -> f64 {
    f64::from(ALPHA_HUNDREDTHS) / 100.0
}

/// The distance between two heads under the navigation metric.
fn head_distance(left: &[u8], right: &[u8], dimension: usize) -> Option<f64> {
    let (scale, codes) = head_codes(left, dimension);
    let (lanes, norm) = widen_codes(scale, codes);
    let (other_scale, other_codes) = head_codes(right, dimension);
    navigation(other_scale, other_codes, &lanes, norm)
}

/// Link one node into the graph.
///
/// COST, stated: ONE greedy search at `BUILD_SEARCH_LIST` (its records read
/// are the search's own bound, not the corpus), then at most `DEGREE`
/// read-modify-writes of ADJACENCY records for the back edges and at most
/// `DEGREE` more for the edges those displaced, plus this node's own head
/// and adjacency and the header. So O(L + R) reads and O(R) writes per
/// insert, with no term in the number of rows and -- because an adjacency
/// record is at most 1,156 bytes -- no term in the DIMENSION beyond the one
/// head this insert writes. The entry-point refresh adds `SAMPLE` reads once
/// per DOUBLING of the graph.
pub(crate) fn link_node(
    db: &mut Database,
    index: &IndexInfo,
    seq: u64,
    head: Vec<u8>,
) -> Result<()> {
    let dimension = dimension(index)?;
    if head.len() != head_len(dimension) {
        return Err(corrupt("vamana node head length"));
    }
    let mut header = read_header(db, index.id)?;
    if header.nodes == 0 {
        put_head(db, index.id, seq, &head)?;
        put_adjacency(db, index.id, seq, &Adjacency::default())?;
        return put_header(
            db,
            index.id,
            GraphHeader {
                entry: seq,
                nodes: 1,
                entry_at: 1,
            },
        );
    }
    let (scale, codes) = head_codes(&head, dimension);
    let (lanes, norm) = widen_codes(scale, codes);
    let mut silent = || false;
    let (nearest, passed, _) = {
        let mut progress = crate::index::vector::exact::cancel_only(&mut silent);
        greedy_search(
            db,
            index,
            dimension,
            &lanes,
            norm,
            NAVIGATION,
            header.entry,
            BUILD_SEARCH_LIST,
            seq,
            usize::MAX,
            BUILD_VISITED,
            &mut progress,
        )?
    };
    // WHAT THE PRUNE WEIGHS, and why it is not the nearest nodes.
    //
    // `passed` is the first `BUILD_VISITED` nodes the walk PASSED THROUGH, in
    // the order it reached them -- so it starts at the entry point and works
    // inward -- sorted by distance before it is offered here. That set is the
    // one that was measured best, and the reason is navigability. RobustPrune
    // accepts in ascending distance and alpha only removes what an accepted
    // neighbour already covers, so a candidate set weighted toward NEAR nodes
    // spends the whole degree budget on short edges and the graph stops being
    // navigable. Measured over 10,000 clustered 128-lane rows, recall@10 at a
    // search list of 200: 0.91 from the `list`-nearest alone, 0.93 from a
    // sweep three times wider, 0.80 from the union of the two, and 0.99 from
    // this set. `nearest` is therefore deliberately NOT added; it is the
    // query path's answer, and it is dropped here.
    let _ = nearest;
    let neighbours = robust_prune(passed, dimension, alpha(), DEGREE);
    let mut pending = Pending::default();
    // The new node goes into the pending set with the rest, so a prune that
    // drops its edge drops it on this side too and the relation stays
    // symmetric with no special case.
    pending.held.insert(
        seq,
        Record {
            head,
            adjacency: Adjacency {
                own: neighbours.clone(),
                back: Vec::new(),
            },
        },
    );
    pending.dirty.insert(seq);
    for edge in &neighbours {
        if !fetch(db, &mut pending, index.id, dimension, edge.seq)? {
            return Err(corrupt("vamana graph neighbour record is missing"));
        }
        pending
            .touch(edge.seq)
            .expect("fetched above")
            .adjacency
            .append(Edge {
                seq,
                distance: edge.distance,
            });
        prune_neighbour(db, index, dimension, &mut pending, edge.seq)?;
    }
    header.nodes += 1;
    if refresh_is_due(header) {
        if let Some(entry) = sample_medoid(db, index, dimension)? {
            header.entry = entry;
            header.entry_at = header.nodes;
        }
    }
    // The HEAD is written here and never again: it is the only record this
    // insert writes whose size is the dimension, and the edge repair above
    // cannot reach it. It lands with the flush, after the entry-point
    // refresh, so the sample this insert takes is over the graph as it was --
    // the same set of nodes the single-record layout sampled.
    let head = pending.get(seq).expect("held above").head.as_slice();
    put_head(db, index.id, seq, head)?;
    pending.flush(db, index.id)?;
    put_header(db, index.id, header)
}

/// Unlink one node, leaving no list naming it.
///
/// COST, stated: at most `DEGREE` reads and `DEGREE` ADJACENCY writes, plus
/// this node's two records and the header, and
/// `DEGREE^2` int8 distances in memory for the consolidation. There is no
/// term in the number of rows, and that is what the SYMMETRIC invariant buys:
/// the lists that name this node are exactly the lists it names, so finding
/// them is a read of its own record rather than a walk of the keyspace.
///
/// CONSOLIDATION. Removing a node leaves its neighbours one edge short and,
/// worse, no longer joined through it. Each of them is therefore offered the
/// closest OTHER member of the departing node's neighbourhood that it does
/// not already hold, which is Vamana's own delete rule taken to one edge so
/// the bound stays in `DEGREE`. The offer is refused when either side is
/// full; a full list is already at its navigable degree.
pub(crate) fn unlink_node(db: &mut Database, index: &IndexInfo, seq: u64) -> Result<()> {
    let dimension = dimension(index)?;
    let Some(record) = read_record(db, index.id, dimension, seq)? else {
        return Ok(());
    };
    let mut header = read_header(db, index.id)?;
    let neighbours: Vec<u64> = record.adjacency.sequences().collect();
    let mut pending = Pending::default();
    for other in &neighbours {
        if fetch(db, &mut pending, index.id, dimension, *other)? {
            pending
                .touch(*other)
                .expect("fetched above")
                .adjacency
                .remove(seq);
        }
    }
    // The consolidation, over the neighbourhood the departing node held
    // together. Distances are computed from the heads already in hand.
    let held: Vec<u64> = neighbours
        .iter()
        .copied()
        .filter(|other| pending.get(*other).is_some())
        .collect();
    for left in &held {
        let mut best: Option<(u64, f64)> = None;
        for right in &held {
            if left == right {
                continue;
            }
            let (l, r) = (
                pending.get(*left).expect("held"),
                pending.get(*right).expect("held"),
            );
            if l.adjacency.holds(*right)
                || l.adjacency.len() >= MAX_DEGREE
                || r.adjacency.len() >= MAX_DEGREE
            {
                continue;
            }
            if let Some(distance) = head_distance(&l.head, &r.head, dimension) {
                if best.is_none_or(|(_, held)| distance < held) {
                    best = Some((*right, distance));
                }
            }
        }
        let Some((right, distance)) = best else {
            continue;
        };
        let edge = distance as f32;
        pending
            .touch(*left)
            .expect("held")
            .adjacency
            .append(Edge { seq: right, distance: edge });
        pending
            .touch(right)
            .expect("held")
            .adjacency
            .append(Edge { seq: *left, distance: edge });
    }
    header.nodes = header.nodes.saturating_sub(1);
    if header.entry == seq {
        header.entry = record
            .adjacency
            .own
            .first()
            .or_else(|| record.adjacency.back.first())
            .map_or(0, |edge| edge.seq);
        if header.entry == 0 && header.nodes > 0 {
            header.entry = first_node(db, index.id, seq)?;
        }
        header.entry_at = header.nodes;
    }
    if header.nodes == 0 {
        header.entry = 0;
        header.entry_at = 0;
    }
    // Both of the node's records go, in this transaction: the two keyspaces
    // hold exactly the same sequences, and a delete that left one behind
    // would be the damage verification is written to find.
    db.writer()?.delete(&node_key(index.id, seq))?;
    db.writer()?.delete(&adjacency_key(index.id, seq))?;
    pending.flush(db, index.id)?;
    put_header(db, index.id, header)
}

/// The first node record of this index other than `skip`: the fallback entry
/// point when a delete removed the entry point of a graph whose neighbour
/// list was empty.
fn first_node(db: &Database, id: IndexId, skip: u64) -> Result<u64> {
    let prefix = node_prefix(id);
    let mut found = 0u64;
    let mut failure = None;
    db.store()?.range(&prefix)?.for_each_ref(|key, _| {
        if !key.starts_with(&prefix) {
            return false;
        }
        let mut at = prefix.len();
        match read_ordered(key, &mut at) {
            Ok(seq) if at == key.len() => {
                if seq != 0 && seq != skip {
                    found = seq;
                    return false;
                }
                true
            }
            _ => {
                failure = Some(corrupt("vamana node key"));
                false
            }
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(found)
}

fn refresh_is_due(header: GraphHeader) -> bool {
    header.nodes >= SAMPLE_FLOOR && header.nodes >= header.entry_at.saturating_mul(2)
}

/// A fresh entry point: the MEDOID of a spread sample of the graph.
///
/// The true medoid needs a pass over every node, which no write path can
/// afford. `SAMPLE` nodes are taken by seeking to evenly spaced points of the
/// index's own key range and keeping the first record at or after each, and
/// the sample member whose summed distance to the rest is least is the entry
/// point. Paid once per doubling of the graph.
fn sample_medoid(db: &Database, index: &IndexInfo, dimension: usize) -> Result<Option<u64>> {
    let prefix = node_prefix(index.id);
    let mut last = 0u64;
    let mut failure = None;
    db.store()?.range(&prefix)?.for_each_ref(|key, _| {
        if !key.starts_with(&prefix) {
            return false;
        }
        let mut at = prefix.len();
        match read_ordered(key, &mut at) {
            Ok(seq) if at == key.len() => {
                last = last.max(seq);
                true
            }
            _ => {
                failure = Some(corrupt("vamana node key"));
                false
            }
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    if last == 0 {
        return Ok(None);
    }
    let mut sample: Vec<(u64, Vec<u8>)> = Vec::with_capacity(SAMPLE);
    let mut seen = HashSet::with_capacity(SAMPLE);
    for step in 0..SAMPLE as u64 {
        let target = 1 + (last.saturating_sub(1)).saturating_mul(step) / SAMPLE as u64;
        let mut key = prefix.clone();
        key.extend(ordered(target));
        let mut found = None;
        let mut failure = None;
        db.store()?.range(&key)?.for_each_ref(|key, value| {
            if !key.starts_with(&prefix) {
                return false;
            }
            let mut at = prefix.len();
            match read_ordered(key, &mut at) {
                Ok(seq) if at == key.len() && seq != 0 => {
                    found = Some((seq, value.to_vec()));
                    false
                }
                Ok(_) => true,
                _ => {
                    failure = Some(corrupt("vamana node key"));
                    false
                }
            }
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        let Some((seq, value)) = found else {
            continue;
        };
        if !seen.insert(seq) {
            continue;
        }
        decode_head(&value, dimension)?;
        sample.push((seq, value));
    }
    let mut best: Option<(u64, f64)> = None;
    for (seq, head) in &sample {
        let (scale, codes) = head_codes(head, dimension);
        let (lanes, norm) = widen_codes(scale, codes);
        let mut total = 0.0f64;
        for (other, other_head) in &sample {
            if other == seq {
                continue;
            }
            let (other_scale, other_codes) = head_codes(other_head, dimension);
            total += navigation(other_scale, other_codes, &lanes, norm).unwrap_or(0.0);
        }
        if best.is_none_or(|(_, held)| total < held) {
            best = Some((*seq, total));
        }
    }
    Ok(best.map(|(seq, _)| seq))
}

/// The write-path hook, called for every row written while a vamana index
/// exists on the collection.
///
/// WHAT A WRITE DOES, stated: an INSERT links the new node (one greedy search
/// plus O(R) record writes); a DELETE unlinks it (O(R) reads and writes); an
/// UPDATE that MOVED the vector is an unlink followed by a link, because a
/// node whose position changed no longer belongs where its edges put it; an
/// update that left the vector alone touches nothing. There is no deferred
/// work and no rebuild, so the graph a query walks is always the graph the
/// committed rows describe -- this index cannot answer from a stale graph
/// because it never holds one.
pub(crate) fn maintain_node(
    db: &mut Database,
    index: &IndexInfo,
    id: EntityId,
    new: Option<(&Layout, &VectorCells)>,
    fresh: bool,
) -> Result<()> {
    let dimension = dimension(index)?;
    let desired = new
        .map(|(layout, vectors)| desired_head(index, layout, vectors))
        .transpose()?
        .flatten();
    if fresh {
        if let Some(head) = desired {
            link_node(db, index, id.sequence, head)?;
        }
        return Ok(());
    }
    let existing = read_record(db, index.id, dimension, id.sequence)?;
    match (existing, desired) {
        (Some(old), Some(head)) if old.head == head => Ok(()),
        (Some(_), Some(head)) => {
            unlink_node(db, index, id.sequence)?;
            link_node(db, index, id.sequence, head)
        }
        (None, Some(head)) => link_node(db, index, id.sequence, head),
        (Some(_), None) => unlink_node(db, index, id.sequence),
        (None, None) => Ok(()),
    }
}

/// The late build's write: link a row the build has reached, unless the live
/// write path already linked it with the same head.
pub(crate) fn build_link(
    db: &mut Database,
    index: &IndexInfo,
    seq: u64,
    head: Vec<u8>,
) -> Result<()> {
    let dimension = dimension(index)?;
    if let Some(existing) = read_record(db, index.id, dimension, seq)? {
        if existing.head == head {
            return Ok(());
        }
        unlink_node(db, index, seq)?;
    }
    link_node(db, index, seq, head)
}

// ── The read path ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct ExactHit(VectorHit);

impl PartialEq for ExactHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.distance.to_bits() == other.0.distance.to_bits() && self.0.id == other.0.id
    }
}
impl Eq for ExactHit {}
impl PartialOrd for ExactHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ExactHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .distance
            .total_cmp(&other.0.distance)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

impl Database {
    /// Create a vamana graph index. The feature bit and the keyspace arrive
    /// together, in the transaction that creates the descriptor: an older
    /// binary refuses the file from that commit onward and never sees a
    /// `0x7D` record it would not maintain (Law 8).
    pub fn create_vamana_index(
        &mut self,
        collection: CollectionId,
        name: &str,
        field: &str,
    ) -> Result<IndexId> {
        self.ready_write()?;
        let info = self.collection_info(collection)?;
        let kind = info
            .layout
            .fields
            .iter()
            .find(|(candidate, _)| candidate == field)
            .map(|(_, kind)| kind.clone())
            .ok_or_else(|| invalid("index field must be declared"))?;
        if !matches!(kind, Kind::Vector(d) if (1..=vector_quant::MAX_DIMENSION).contains(&d)) {
            return Err(invalid("vamana graph index requires a vector field"));
        }
        self.create_index(
            collection,
            name,
            field,
            kind,
            false,
            IndexFamily::VamanaGraph,
            VAMANA_FEATURE,
        )
    }

    /// Approximate top-k over the graph. `ef` is the search list: the caller
    /// sets it with `SET LOCAL diskann.query_search_list_size` (or its
    /// `ef_search` spelling), and recall rises with it.
    #[allow(clippy::too_many_arguments)]
    pub fn query_vamana_vector(
        &self,
        id: IndexId,
        query: &[f32],
        metric: VectorMetric,
        k: usize,
        ef: usize,
        max_examined: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<ApproxVectorResult> {
        let mut progress = crate::index::vector::exact::cancel_only(&mut cancelled);
        self.scan_vamana(id, query, metric, k, ef, None, max_examined, &mut progress)
    }

    /// The paged form: the same walk, with the page cursor applied to the
    /// RERANKED order and a progress hook the caller charges through.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scan_vamana(
        &self,
        id: IndexId,
        query: &[f32],
        metric_value: VectorMetric,
        k: usize,
        ef: usize,
        after: Option<crate::index::vector::exact::VectorAfter>,
        max_examined: usize,
        progress: crate::index::vector::exact::ScanProgress<'_>,
    ) -> Result<ApproxVectorResult> {
        if k == 0 || k > ef || ef > catalog::MAX_RESULTS {
            return Err(invalid("vamana graph search requires 1 <= k <= ef <= 65536"));
        }
        let index = self.index_info(id)?;
        if index.family != IndexFamily::VamanaGraph {
            return Err(invalid("index is not a vamana graph index"));
        }
        // A graph is not maintained by the BUILD the way a scan's entries
        // are: until the build has reached every row the graph holds only
        // part of the corpus, and a nearest-neighbour answer over part of a
        // corpus is a wrong answer, not a partial one. It is refused by name
        // rather than served (QL_CONTRACT §6).
        if index.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        let dimension = dimension(&index)?;
        if query.len() != dimension || query.iter().any(|lane| !lane.is_finite()) {
            return Err(invalid(
                "query vector has wrong dimension or non-finite lane",
            ));
        }
        let query_norm = query.iter().fold(0.0f64, |sum, lane| {
            sum + f64::from(*lane) * f64::from(*lane)
        });
        if metric_value == VectorMetric::Cosine && query_norm == 0.0 {
            return Err(invalid("cosine query vector must have nonzero norm"));
        }
        progress(crate::index::vector::exact::ScanStep::Scored(0))?;
        let header = read_header(self, id)?;
        if header.nodes == 0 || header.entry == 0 {
            return Ok(ApproxVectorResult {
                hits: Vec::new(),
                method: ApproxVectorMethod::VamanaGraphV1,
                ef,
                examined: 0,
                reranked: 0,
            });
        }
        let (wide, wide_norm) = vector_quant::widen_query(query)
            .map_err(|_| invalid("query vector has wrong dimension or non-finite lane"))?;
        let (shortlist, _, examined) = greedy_search(
            self,
            &index,
            dimension,
            &wide,
            wide_norm,
            metric(metric_value),
            header.entry,
            ef,
            0,
            max_examined,
            0,
            progress,
        )?;
        let reranked = shortlist.len();
        let mut cancelled = || progress(crate::index::vector::exact::ScanStep::Scored(0)).is_err();
        let hits = rerank(
            self,
            &index,
            shortlist,
            dimension,
            query,
            query_norm,
            metric_value,
            k,
            after,
            &mut cancelled,
        )?;
        Ok(ApproxVectorResult {
            hits,
            method: ApproxVectorMethod::VamanaGraphV1,
            ef,
            examined,
            reranked,
        })
    }
}

/// The exact rerank: the same one the quantized family does, over the
/// shortlist the graph walk produced instead of the one a scan produced.
///
/// The head each shortlist entry carries is a DERIVED copy of an immutable
/// f32 sidecar, and the walk that produced it checked its LENGTH and codec
/// and nothing more. So the head is re-derived from the sidecar it names and
/// compared, for the `ef` winners and never for the corpus: without that, a
/// node record whose scale, lanes or locator were rewritten is scored as an
/// approximation of a vector it no longer approximates, and the damage
/// reaches the caller as an answer rather than as `Corrupt` (Law 5).
#[allow(clippy::too_many_arguments)]
fn rerank(
    db: &Database,
    index: &IndexInfo,
    mut shortlist: Vec<Node>,
    dimension: usize,
    query: &[f32],
    query_norm: f64,
    metric: VectorMetric,
    k: usize,
    after: Option<crate::index::vector::exact::VectorAfter>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Vec<VectorHit>> {
    shortlist.sort_unstable_by_key(|node| node.seq);
    let store = db.store()?;
    let mut exact = BinaryHeap::with_capacity(k.min(1024));
    for node in shortlist {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let locator: [u8; 6] = node.head[..6].try_into().unwrap();
        let ordinal = validate_locator(db, index, &locator)?;
        let id = EntityId {
            collection: index.collection,
            sequence: node.seq,
        };
        let raw = store
            .get(&vector_key(id, ordinal))?
            .ok_or_else(|| corrupt("vamana node locator points to missing sidecar"))?;
        if crate::index::vector::quantized::encode_entry(locator, &raw, dimension)? != node.head {
            return Err(corrupt(
                "vamana node head differs from authoritative sidecar",
            ));
        }
        let Some(distance) = crate::index::vector::quantized::exact_score(
            &raw, dimension, query, query_norm, metric, cancelled,
        )?
        else {
            continue;
        };
        if after.is_some_and(|after| !after.admits(distance, id)) {
            continue;
        }
        exact.push(ExactHit(VectorHit { id, distance }));
        if exact.len() > k {
            exact.pop();
        }
    }
    let mut hits: Vec<_> = exact.into_iter().map(|hit| hit.0).collect();
    hits.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(hits)
}
