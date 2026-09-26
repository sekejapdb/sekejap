//! What one row of a GQL working table holds, and the three questions the
//! operators ask of it (`docs/lang/GQL_PROFILE_DESIGN.md` §2.1, §2.2):
//!
//! * **Is it the same element?** [`BindingValue::identity_eq`]: nodes by
//!   `EntityId`, edges by stored key plus their per-key edge id, paths by their steps;
//!   scalars by SQL equality, and anything compared with `Null` is unknown.
//! * **Does it group with that one?** `Eq` and `Hash` on [`BindingValue`]:
//!   the same identities, except that `Null` groups with `Null` (the SQL
//!   `GROUP BY` rule). `DISTINCT`, grouping and `COUNT(DISTINCT x)` use it.
//! * **Which comes first?** `Ord` on [`BindingValue`]: one internal total
//!   order, consistent with `Eq`, for stable ties and ordered sets. It is
//!   deterministic, not meaningful across kinds; a user-visible `ORDER BY`
//!   is typed by the binder (design Q12), not by this order.
//!
//! [`BindingValue::held_bytes`] and [`BindingRow::held_bytes`] are the stated
//! byte estimate every memory charge uses.
//!
//! A path is a shared-prefix (cons) list: extending one allocates one step
//! and points at the whole prefix, so k partial paths hold O(k) steps, not
//! O(k * length). The byte estimate knows this and counts a shared step once.

use crate::collections::{CollectionId, EntityId};
use crate::index::graph::{EdgeKey, EdgeTypeId};
use serde_json::{Number, Value};
use std::{
    cmp::Ordering,
    collections::HashSet,
    hash::{Hash, Hasher},
    mem::size_of,
    sync::{Arc, OnceLock},
};

// ── elements ──────────────────────────────────────────────────────────────

/// A node: a row in a collection. Identity is the `EntityId`, collection
/// plus sequence, so two collections holding the same external key are two
/// nodes. Delete then reinsert allocates a new sequence, so a `NodeRef` can
/// never silently point at a reinserted row. It is valid only inside the
/// execution that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeRef(pub EntityId);

// `EntityId` does not derive `Hash`; hashing its two parts here keeps this
// module from widening a public type another task also edits.
impl Hash for NodeRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_entity(self.0, state);
    }
}

fn hash_entity<H: Hasher>(id: EntityId, state: &mut H) {
    id.collection.0.hash(state);
    id.sequence.hash(state);
}

/// A stored edge. Identity is the stored key PLUS the edge id, so parallel
/// edges of one type between one pair are distinct.
///
/// `key` is the STORED orientation (source to destination), never the
/// traversal orientation; the traversal orientation lives on the path step.
/// `bag` caches the property bag when the operator that produced the
/// reference had already decoded it. Equality, ordering and hashing ignore
/// `bag`: a cached copy is not identity.
///
/// Not to be confused with the reaching edge a BFS binds,
/// `collections::EdgeRef`, which is unchanged.
#[derive(Clone, Debug)]
pub struct EdgeRef {
    pub key: EdgeKey,
    /// The edge's own id within its key: `collections::EdgeId::id`. Edges
    /// written by `put_edge` (and every edge of a file without
    /// `EDGE_ID_FEATURE`) have id 0; `create_edge` gives each parallel edge
    /// its own.
    pub id: u64,
    pub bag: Option<Arc<Value>>,
}

impl PartialEq for EdgeRef {
    fn eq(&self, other: &Self) -> bool {
        (self.key, self.id) == (other.key, other.id)
    }
}

impl Eq for EdgeRef {}

impl PartialOrd for EdgeRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EdgeRef {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.key, self.id).cmp(&(other.key, other.id))
    }
}

impl Hash for EdgeRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_entity(self.key.source, state);
        self.key.context.0.hash(state);
        self.key.edge_type.0.hash(state);
        hash_entity(self.key.destination, state);
        self.id.hash(state);
    }
}

/// A path: one start node and zero or more steps, held as a shared-prefix
/// list. A zero-length path is one node and no edge.
///
/// Identity is the start node and the sequence of (edge, orientation) steps.
/// The node each step arrives at follows from those two.
#[derive(Clone, Debug)]
pub struct PathRef(Arc<PathNode>);

#[derive(Debug)]
enum PathNode {
    Start {
        node: NodeRef,
    },
    Step {
        parent: Arc<PathNode>,
        edge: EdgeRef,
        /// True when the pattern crossed the edge source to destination.
        forward: bool,
        /// The node the step arrives at.
        node: NodeRef,
        /// Edges so far, so the length is O(1).
        len: u32,
    },
}

#[allow(clippy::len_without_is_empty)] // a zero-length path is not empty: it holds a node
impl PathRef {
    /// The zero-length path at `start`.
    pub fn new(start: NodeRef) -> Self {
        Self(Arc::new(PathNode::Start { node: start }))
    }

    /// This path plus one step over `edge` to `node`, crossed source to
    /// destination when `forward`. The prefix is shared, not copied, and this
    /// path is left as it was.
    pub fn extend(&self, edge: EdgeRef, forward: bool, node: NodeRef) -> Self {
        let (from, to) = if forward {
            (edge.key.source, edge.key.destination)
        } else {
            (edge.key.destination, edge.key.source)
        };
        debug_assert!(
            from == self.end().0 && to == node.0,
            "a step must leave the path's end and arrive at `node` along `edge`"
        );
        Self(Arc::new(PathNode::Step {
            parent: Arc::clone(&self.0),
            edge,
            forward,
            node,
            len: self.len() + 1,
        }))
    }

    /// The number of edges.
    pub fn len(&self) -> u32 {
        match &*self.0 {
            PathNode::Start { .. } => 0,
            PathNode::Step { len, .. } => *len,
        }
    }

    /// The first node. O(length).
    pub fn start(&self) -> NodeRef {
        let mut at = &*self.0;
        loop {
            match at {
                PathNode::Start { node } => return *node,
                PathNode::Step { parent, .. } => at = parent,
            }
        }
    }

    /// The last node.
    pub fn end(&self) -> NodeRef {
        match &*self.0 {
            PathNode::Start { node } | PathNode::Step { node, .. } => *node,
        }
    }

    /// The nodes in path order, the start first: `len() + 1` of them, a
    /// node the path revisits listed at each visit. O(length).
    pub fn nodes(&self) -> Vec<NodeRef> {
        let mut nodes = Vec::with_capacity(self.len() as usize + 1);
        let mut at = &*self.0;
        loop {
            match at {
                PathNode::Start { node } => {
                    nodes.push(*node);
                    nodes.reverse();
                    return nodes;
                }
                PathNode::Step { parent, node, .. } => {
                    nodes.push(*node);
                    at = parent;
                }
            }
        }
    }

    /// The edges in path order, each in its STORED orientation (the
    /// direction a step crossed it is the path's, not the edge's). O(length).
    pub fn edges(&self) -> Vec<EdgeRef> {
        let (_, steps) = self.in_order();
        steps.into_iter().map(|(edge, _)| edge.clone()).collect()
    }

    /// The start and the steps in path order, for the total order.
    fn in_order(&self) -> (NodeRef, Vec<(&EdgeRef, bool)>) {
        let mut steps = Vec::with_capacity(self.len() as usize);
        let mut at = &*self.0;
        loop {
            match at {
                PathNode::Start { node } => {
                    steps.reverse();
                    return (*node, steps);
                }
                PathNode::Step {
                    parent,
                    edge,
                    forward,
                    ..
                } => {
                    steps.push((edge, *forward));
                    at = parent;
                }
            }
        }
    }

    /// Does the path already cross `edge` (by identity), in either
    /// orientation? The `TRAIL` check. O(length).
    pub(super) fn has_edge(&self, edge: &EdgeRef) -> bool {
        let mut at = &*self.0;
        while let PathNode::Step {
            parent, edge: e, ..
        } = at
        {
            if e == edge {
                return true;
            }
            at = parent;
        }
        false
    }

    /// Does the path already visit `node`, its start included? The
    /// `ACYCLIC` check. O(length).
    pub(super) fn has_node(&self, node: NodeRef) -> bool {
        let mut at = &*self.0;
        loop {
            match at {
                PathNode::Start { node: n } => return *n == node,
                PathNode::Step { parent, node: n, .. } => {
                    if *n == node {
                        return true;
                    }
                    at = parent;
                }
            }
        }
    }

    /// Bytes of this path's steps not yet in `seen`, marking them seen. The
    /// walk stops at the first step already counted: everything behind it is
    /// a prefix some earlier path in the same count already paid for.
    fn unshared_bytes(&self, seen: &mut HashSet<*const PathNode>) -> u64 {
        let mut total = 0;
        let mut at = &self.0;
        while seen.insert(Arc::as_ptr(at)) {
            total += PATH_NODE_BYTES;
            match &**at {
                PathNode::Start { .. } => break,
                PathNode::Step { parent, edge, .. } => {
                    total += bag_bytes(edge);
                    at = parent;
                }
            }
        }
        total
    }
}

/// A path is dropped step by step, not recursively: dropping the last
/// holder of a step would otherwise drop its parent from inside its own
/// drop, one stack frame per step, and a trail search builds paths as long
/// as its `queue_entries` cap allows. Each step this holder alone owns is
/// unlinked from its parent before it is freed; the walk stops at the first
/// step another path still shares.
impl Drop for PathRef {
    fn drop(&mut self) {
        let mut at = std::mem::replace(&mut self.0, unlinked());
        while let Some(PathNode::Step { parent, .. }) = Arc::get_mut(&mut at) {
            let parent = std::mem::replace(parent, unlinked());
            // Frees `at`, whose parent is now the placeholder.
            at = parent;
        }
    }
}

/// The placeholder a step is unlinked to while its path is dropped: one
/// shared start, so unlinking allocates nothing.
fn unlinked() -> Arc<PathNode> {
    static UNLINKED: OnceLock<Arc<PathNode>> = OnceLock::new();
    Arc::clone(UNLINKED.get_or_init(|| {
        Arc::new(PathNode::Start {
            node: NodeRef(EntityId {
                collection: CollectionId(0),
                sequence: 0,
            }),
        })
    }))
}

impl PartialEq for PathRef {
    fn eq(&self, other: &Self) -> bool {
        let (mut a, mut b) = (&*self.0, &*other.0);
        loop {
            // A shared prefix is equal to itself without walking it.
            if std::ptr::eq(a, b) {
                return true;
            }
            match (a, b) {
                (PathNode::Start { node: x }, PathNode::Start { node: y }) => return x == y,
                (
                    PathNode::Step {
                        parent: pa,
                        edge: ea,
                        forward: fa,
                        len: la,
                        ..
                    },
                    PathNode::Step {
                        parent: pb,
                        edge: eb,
                        forward: fb,
                        len: lb,
                        ..
                    },
                ) => {
                    if la != lb || fa != fb || ea != eb {
                        return false;
                    }
                    (a, b) = (pa, pb);
                }
                _ => return false,
            }
        }
    }
}

impl Eq for PathRef {}

impl PartialOrd for PathRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Start node first, then the steps in path order, a prefix before its
/// extensions.
impl Ord for PathRef {
    fn cmp(&self, other: &Self) -> Ordering {
        if Arc::ptr_eq(&self.0, &other.0) {
            return Ordering::Equal;
        }
        let (sa, a) = self.in_order();
        let (sb, b) = other.in_order();
        sa.cmp(&sb).then_with(|| a.cmp(&b))
    }
}

/// The same sequence `eq` compares, read tip first.
impl Hash for PathRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        let mut at = &*self.0;
        loop {
            match at {
                PathNode::Start { node } => return node.hash(state),
                PathNode::Step {
                    parent,
                    edge,
                    forward,
                    ..
                } => {
                    edge.hash(state);
                    forward.hash(state);
                    at = parent;
                }
            }
        }
    }
}

// ── values, lists and rows ────────────────────────────────────────────────

/// One slot value. Inside the profile a missing property and a stored null
/// both read as `Null`.
#[derive(Clone, Debug)]
pub enum BindingValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(Arc<str>),
    Json(Arc<Value>),
    Vector(Arc<[f32]>),
    /// A stored geometry, as GeoJSON (what a geometry column stores).
    Geo(Arc<Value>),
    Bytes(Arc<[u8]>),
    Node(NodeRef),
    Edge(EdgeRef),
    Path(PathRef),
    List(ListRef),
}

/// A typed, ordered list. Never flattened, never deduplicated implicitly.
/// `elem` is the element type the binder proved. Equality, order and hash
/// read the items only: two lists of one binding slot share one `elem`.
#[derive(Clone, Debug)]
pub struct ListRef {
    pub items: Arc<[BindingValue]>,
    pub elem: ValueType,
}

/// The type of a slot, as the binder proves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// The `NULL` literal before unification.
    Unknown,
    Bool,
    Int,
    Float,
    Text,
    Json,
    /// A vector, with its dimension when the binder knows it.
    Vector(Option<u32>),
    Geo,
    Bytes,
    /// Declared spellings over `Int` (QL deviation 8).
    Timestamp,
    Date,
    /// A node, and the collections (label alternatives) it can be in.
    Node(Box<[CollectionId]>),
    /// An edge, and the edge types it can be; empty means any type.
    Edge(Box<[EdgeTypeId]>),
    Path,
    List(Box<ValueType>),
}

/// A row of the working table: `slots[i]` is slot `SlotId(i)` of the stage
/// schema that produced it. The schema is the language layer's; the engine
/// knows only the width.
#[derive(Clone, Debug)]
pub struct BindingRow {
    pub slots: Box<[BindingValue]>,
}

/// A slot's position in a [`BindingRow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u16);

impl BindingRow {
    pub fn get(&self, slot: SlotId) -> &BindingValue {
        &self.slots[usize::from(slot.0)]
    }

    /// The stated estimate of what this row holds: every slot's
    /// [`BindingValue::held_bytes`], except that a path step two slots share
    /// is counted once.
    pub fn held_bytes(&self) -> u64 {
        let mut seen = HashSet::new();
        self.slots
            .iter()
            .map(|value| SLOT_BYTES + value.heap_bytes(&mut seen))
            .sum()
    }
}

impl BindingValue {
    /// Identity equality under three-valued logic: `None` is unknown.
    ///
    /// Node: `EntityId` equal. Edge: stored key and edge id equal. Path:
    /// equal start and equal (edge, orientation) steps. Scalars: SQL
    /// equality, numbers as numbers. Lists: item by item, false as soon as
    /// one item is false, unknown if none is false and one is unknown.
    /// `Null` against anything is unknown. Values of different kinds are
    /// not equal (the binder refuses such comparisons before they run).
    pub fn identity_eq(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (Self::Null, _) | (_, Self::Null) => None,
            (Self::List(a), Self::List(b)) => {
                if a.items.len() != b.items.len() {
                    return Some(false);
                }
                let mut unknown = false;
                for (x, y) in a.items.iter().zip(b.items.iter()) {
                    match x.identity_eq(y) {
                        Some(false) => return Some(false),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                if unknown { None } else { Some(true) }
            }
            _ => Some(self == other),
        }
    }

    /// The stated estimate of what this value holds, in bytes: one slot
    /// (`size_of::<BindingValue>()`) plus the heap behind text, JSON,
    /// geometry, vector, bytes, a cached edge bag, a list's items and a
    /// path's steps, each `Arc` with its two counters. A path step shared by
    /// two paths inside this value is counted once.
    pub fn held_bytes(&self) -> u64 {
        SLOT_BYTES + self.heap_bytes(&mut HashSet::new())
    }

    fn heap_bytes(&self, seen: &mut HashSet<*const PathNode>) -> u64 {
        match self {
            Self::Null | Self::Bool(_) | Self::Int(_) | Self::Float(_) | Self::Node(_) => 0,
            Self::Text(text) => ARC_BYTES + text.len() as u64,
            Self::Json(value) | Self::Geo(value) => json_bytes(value),
            Self::Vector(lanes) => ARC_BYTES + (lanes.len() * size_of::<f32>()) as u64,
            Self::Bytes(bytes) => ARC_BYTES + bytes.len() as u64,
            Self::Edge(edge) => bag_bytes(edge),
            Self::Path(path) => path.unshared_bytes(seen),
            Self::List(list) => {
                let items: u64 = list.items.iter().map(|v| v.heap_bytes(seen)).sum();
                ARC_BYTES + list.items.len() as u64 * SLOT_BYTES + items
            }
        }
    }

    /// The kind's place in the cross-kind order: `Null` < Bool < numbers <
    /// Text < Json < the rest by this tag.
    fn rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Bool(_) => 1,
            Self::Int(_) | Self::Float(_) => 2,
            Self::Text(_) => 3,
            Self::Json(_) => 4,
            Self::Vector(_) => 5,
            Self::Geo(_) => 6,
            Self::Bytes(_) => 7,
            Self::Node(_) => 8,
            Self::Edge(_) => 9,
            Self::Path(_) => 10,
            Self::List(_) => 11,
        }
    }
}

/// Grouping equality: `cmp` is `Equal`. So `Null` equals `Null`, and `Int(1)`
/// equals `Float(1.0)`.
impl PartialEq for BindingValue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for BindingValue {}

impl PartialOrd for BindingValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The internal total order. Across kinds, [`BindingValue::rank`]. Within a
/// kind, the natural order: numbers exactly (an `Int` against a `Float`
/// without rounding either, `-0.0` equal to `0.0`, NaN equal to itself and
/// after every other number); text by bytes; JSON by kind, then value, with
/// object keys compared sorted; vectors and lists item by item, a prefix
/// first; elements by identity.
impl Ord for BindingValue {
    fn cmp(&self, other: &Self) -> Ordering {
        use BindingValue as V;
        self.rank().cmp(&other.rank()).then_with(|| match (self, other) {
            (V::Bool(a), V::Bool(b)) => a.cmp(b),
            (V::Int(_) | V::Float(_), V::Int(_) | V::Float(_)) => {
                cmp_num(self.number(), other.number())
            }
            (V::Text(a), V::Text(b)) => a.cmp(b),
            (V::Json(a), V::Json(b)) | (V::Geo(a), V::Geo(b)) => cmp_json(a, b),
            (V::Vector(a), V::Vector(b)) => {
                cmp_lex(a, b, |x, y| cmp_float(f64::from(*x), f64::from(*y)))
            }
            (V::Bytes(a), V::Bytes(b)) => a.cmp(b),
            (V::Node(a), V::Node(b)) => a.cmp(b),
            (V::Edge(a), V::Edge(b)) => a.cmp(b),
            (V::Path(a), V::Path(b)) => a.cmp(b),
            (V::List(a), V::List(b)) => a.items.iter().cmp(b.items.iter()),
            // Equal ranks are equal kinds, and Null has nothing to compare.
            _ => Ordering::Equal,
        })
    }
}

/// Consistent with `Eq`: equal values hash alike, including `Int(1)` and
/// `Float(1.0)`, and JSON objects whatever their key order.
impl Hash for BindingValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.rank().hash(state);
        match self {
            Self::Null => {}
            Self::Bool(b) => b.hash(state),
            Self::Int(_) | Self::Float(_) => hash_num(self.number(), state),
            Self::Text(text) => text.hash(state),
            Self::Json(value) | Self::Geo(value) => hash_json(value, state),
            Self::Vector(lanes) => {
                lanes.len().hash(state);
                for lane in lanes.iter() {
                    hash_num(Num::Float(f64::from(*lane)), state);
                }
            }
            Self::Bytes(bytes) => bytes.hash(state),
            Self::Node(node) => node.hash(state),
            Self::Edge(edge) => edge.hash(state),
            Self::Path(path) => path.hash(state),
            Self::List(list) => {
                list.items.len().hash(state);
                for item in list.items.iter() {
                    item.hash(state);
                }
            }
        }
    }
}

impl BindingValue {
    fn number(&self) -> Num {
        match self {
            Self::Int(i) => Num::Int(i128::from(*i)),
            Self::Float(f) => Num::Float(*f),
            _ => Num::Float(f64::NAN),
        }
    }
}

// ── numbers: one exact order and a hash that agrees with it ───────────────

/// A number as the order sees it. `i128` holds every `i64` and every `u64`
/// (a JSON number may be either), so integers never round.
#[derive(Clone, Copy)]
enum Num {
    Int(i128),
    Float(f64),
}

/// 2^127: every float at or past it is larger than any `i128`.
const I128_BOUND: f64 = (1u128 << 127) as f64;

fn cmp_float(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        // Neither is NaN, so this is total; -0.0 and 0.0 are equal.
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// An integer against a float, exactly: compare the integer with the
/// float's integral part, then let the fraction break the tie.
fn cmp_int_float(i: i128, f: f64) -> Ordering {
    if f.is_nan() || f >= I128_BOUND {
        return Ordering::Less;
    }
    if f < -I128_BOUND {
        return Ordering::Greater;
    }
    let whole = f.trunc();
    i.cmp(&(whole as i128)).then_with(|| cmp_float(whole, f))
}

fn cmp_num(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x.cmp(&y),
        (Num::Float(x), Num::Float(y)) => cmp_float(x, y),
        (Num::Int(x), Num::Float(y)) => cmp_int_float(x, y),
        (Num::Float(x), Num::Int(y)) => cmp_int_float(y, x).reverse(),
    }
}

/// An integral float hashes as the integer it equals; NaN as one value;
/// every other float by its bits.
fn hash_num<H: Hasher>(n: Num, state: &mut H) {
    match n {
        Num::Int(i) => {
            0u8.hash(state);
            i.hash(state);
        }
        Num::Float(f) if f.is_nan() => 1u8.hash(state),
        Num::Float(f) if f.fract() == 0.0 && f.abs() < I128_BOUND => {
            0u8.hash(state);
            (f as i128).hash(state);
        }
        Num::Float(f) => {
            2u8.hash(state);
            f.to_bits().hash(state);
        }
    }
}

fn cmp_lex<T>(a: &[T], b: &[T], cmp: impl Fn(&T, &T) -> Ordering) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = cmp(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

// ── JSON: the same order and hash, recursively ────────────────────────────

fn json_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

fn json_num(n: &Number) -> Num {
    match (n.as_i64(), n.as_u64()) {
        (Some(i), _) => Num::Int(i128::from(i)),
        (None, Some(u)) => Num::Int(i128::from(u)),
        (None, None) => Num::Float(n.as_f64().unwrap_or(f64::NAN)),
    }
}

/// An object's entries sorted by key. `serde_json`'s map is sorted already
/// unless a crate in the build turns on `preserve_order`; sorting here keeps
/// equality independent of which.
fn sorted(map: &serde_json::Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

fn cmp_json(a: &Value, b: &Value) -> Ordering {
    json_rank(a).cmp(&json_rank(b)).then_with(|| match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => cmp_num(json_num(x), json_num(y)),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => cmp_lex(x, y, cmp_json),
        (Value::Object(x), Value::Object(y)) => cmp_lex(&sorted(x), &sorted(y), |p, q| {
            p.0.cmp(q.0).then_with(|| cmp_json(p.1, q.1))
        }),
        _ => Ordering::Equal,
    })
}

fn hash_json<H: Hasher>(v: &Value, state: &mut H) {
    json_rank(v).hash(state);
    match v {
        Value::Null => {}
        Value::Bool(b) => b.hash(state),
        Value::Number(n) => hash_num(json_num(n), state),
        Value::String(s) => s.hash(state),
        Value::Array(items) => {
            items.len().hash(state);
            for item in items {
                hash_json(item, state);
            }
        }
        Value::Object(map) => {
            map.len().hash(state);
            for (key, value) in sorted(map) {
                key.hash(state);
                hash_json(value, state);
            }
        }
    }
}

// ── the byte estimate ─────────────────────────────────────────────────────

/// One slot, whatever it holds inline. The design's sketch said 16; an
/// inline `EdgeRef` makes the enum larger, and the estimate states the real
/// size rather than a smaller one.
const SLOT_BYTES: u64 = size_of::<BindingValue>() as u64;

/// An `Arc`'s strong and weak counters, in front of every shared allocation.
const ARC_BYTES: u64 = 2 * size_of::<usize>() as u64;

/// One path step or start, with its `Arc` counters.
const PATH_NODE_BYTES: u64 = ARC_BYTES + size_of::<PathNode>() as u64;

fn json_bytes(value: &Value) -> u64 {
    ARC_BYTES + json_tree_bytes(value)
}

/// A JSON value's own node plus what it owns: string bytes, array items,
/// and per object entry the key's `String` and bytes.
fn json_tree_bytes(value: &Value) -> u64 {
    let own = size_of::<Value>() as u64;
    own + match value {
        Value::String(s) => s.len() as u64,
        Value::Array(items) => items.iter().map(json_tree_bytes).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| size_of::<String>() as u64 + k.len() as u64 + json_tree_bytes(v))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn bag_bytes(edge: &EdgeRef) -> u64 {
    edge.bag.as_deref().map_or(0, json_bytes)
}
