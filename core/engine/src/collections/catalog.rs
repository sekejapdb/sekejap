//! Versioned index catalog and scalar access path over the existing page-WAL.
//! No private cache, background work, implicit commits or entity-format changes.
use super::*;
use crate::scalar_key;
const MAGIC: &[u8; 8] = b"E4IDX01\0";
pub(super) const DESCRIPTOR: u8 = 3;
pub(super) const REGISTRY: u8 = 4;
pub(super) const COLLECTION_INDEX: u8 = 5;
pub(super) const INDEX_NAME: u8 = 0x11;
pub(crate) const SCALAR: u8 = 0x70;
pub(super) const MAX_INDEXES: usize = 64;
/// First (largest) commit group a sorted build tries. A version-2 index packs
/// its first run at this group and only at this group; a later attempt, after
/// an allowance refusal halved it, takes the ascending-put path instead.
///
/// It is NOT the ascending phase's commit cadence. It used to be, and a chunk
/// count is not a transaction size: see `build_sorted_once`, which ends its
/// runs on `sorted_run_budget` and scales that budget by the group the
/// back-off has reached.
pub(super) const SORTED_GROUP_FIRST: usize = 64;
/// The collection header bit that says this database contains at least one
/// index with its OWN B-tree. Monotone and set only by a CREATE that allocates
/// a tree; opening or writing never sets it, so a database of shared-tree
/// indexes stays readable and writable by every binary that predates this
/// work, and one that does contain a per-index tree is refused whole by such a
/// binary before a byte of it is touched (Law 8).
pub(super) const INDEX_TREE_FEATURE: u64 = 0x80;
/// The collection header bit that says this database contains at least one
/// EXPRESSION index -- a scalar index whose stored value is a closed function
/// of a declared field rather than the field itself (descriptor version 3).
///
/// Monotone and set only by a CREATE that records an expression. A binary
/// that predates this work refuses such a file whole at admission rather than
/// reading the index as an ordinary one over the source field, which would
/// answer `col = 'Home'` from keys that hold `'home'` (Law 8).
pub(super) const EXPRESSION_FEATURE: u64 = 0x400;
/// Default for new `Database` handles: whether an index CREATED through that
/// handle gets its own tree. Like the cell-encoding create switch, it decides
/// what is created, never what can be opened. Both layouts are read and
/// written by this binary. Creation itself reads the handle, never this.
static CREATE_INDEX_TREES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
/// Read the per-index-tree creation default copied onto new handles.
pub fn create_index_trees() -> bool {
    CREATE_INDEX_TREES.load(std::sync::atomic::Ordering::Relaxed)
}
/// Set the default copied onto handles created from now on, returning the
/// previous default. Existing handles and existing indexes are unaffected.
pub fn set_create_index_trees(on: bool) -> bool {
    CREATE_INDEX_TREES.swap(on, std::sync::atomic::Ordering::Relaxed)
}
/// Which families this handle gives their own tree when its create-index-trees
/// setting is on. Text, vector, quantized-vector, graph, rows, the name and
/// collection mappings and the vector sidecars all stay in the primary tree:
/// they are out of scope for this change and their descriptors stay at version 1.
fn own_tree(family: IndexFamily, on: bool) -> bool {
    on && matches!(family, IndexFamily::Scalar | IndexFamily::SpatialPoint)
}
/// A scalar or spatial index's own B-tree: the kernel tree id its pages are
/// stamped with, and the root page that tree is reached through.
///
/// `root == 0` is an empty tree -- no page allocated yet. The root moves
/// whenever the tree's height grows, and the descriptor holding it is written
/// in the same commit as the pages that moved it, so the two can never
/// disagree across a crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexTree {
    pub id: u16,
    pub root: u32,
}
pub(super) const MAX_BATCH: usize = 256;
pub(crate) const MAX_RESULTS: usize = 65536;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexId(pub u64);
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexState {
    Building { after: u64 },
    Ready,
    Dropping,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexFamily {
    Scalar,
    ExactVector,
    SpatialPoint,
    Text,
    QuantizedVector,
    SpatialGeometry,
}
#[derive(Clone, Debug, PartialEq)]
pub struct IndexInfo {
    pub id: IndexId,
    pub collection: CollectionId,
    pub name: String,
    pub field: String,
    pub family: IndexFamily,
    pub kind: Kind,
    pub unique: bool,
    pub state: IndexState,
    pub encoding_version: u16,
    /// `Some` only for a version-2 scalar or spatial descriptor: every other
    /// family, and every version-1 descriptor, lives in the primary tree.
    pub tree: Option<IndexTree>,
    /// `Some` for an EXPRESSION index: the index stores `expression(field)`
    /// rather than `field` itself. Scalar family only, descriptor version 3,
    /// behind [`EXPRESSION_FEATURE`].
    ///
    /// `field` stays the SOURCE field, so the layout check
    /// (`validate_indexed_layout`), the late build (`scalar_build_key`) and
    /// the per-write hook (`maintain_indexes`) all read the same declared
    /// field they always did; only the VALUE they encode passes through
    /// [`IndexExpr::apply`] first. No row byte changes: the derived value
    /// lives in the index and nowhere else.
    pub expression: Option<IndexExpr>,
}
/// The expression an expression index stores.
///
/// The set is CLOSED and each member is O(value bytes) per write, which is
/// what keeps the per-write hook bounded (Law 1). `Lower` is the one
/// `docs/lang/QL_CONTRACT.md` §4.1 names: `lower(col) = x` rewrites to a scalar
/// range only when an index over `lower(col)` exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexExpr {
    Lower,
}
impl IndexExpr {
    pub fn written(self) -> &'static str {
        match self {
            Self::Lower => "lower",
        }
    }
    fn byte(self) -> u8 {
        match self {
            Self::Lower => 1,
        }
    }
    fn from_byte(b: u8) -> Result<Option<Self>> {
        match b {
            0 => Ok(None),
            1 => Ok(Some(Self::Lower)),
            _ => Err(Error::Unsupported("index expression".into())),
        }
    }
    /// The derived value this index stores for a source value.
    ///
    /// Analyzer-free and locale-free on purpose: `to_lowercase` is Unicode
    /// simple lowercase, the same folding `str::to_lowercase` gives the row
    /// function in `lang/src/functions.rs`, so the index and the row path
    /// cannot disagree about what `lower(col)` is. A non-string value passes
    /// through unchanged -- `lower` of a number is the number.
    pub(crate) fn apply(self, value: Option<&Value>) -> Option<Value> {
        match (self, value) {
            (Self::Lower, Some(Value::String(s))) => Some(Value::String(s.to_lowercase())),
            (_, other) => other.cloned(),
        }
    }
}
#[derive(Clone, Debug)]
pub enum ScalarPredicate {
    Eq(Value),
    /// Inclusive bounds; None is unbounded. Results sort by value, then entity ID.
    Range {
        lower: Option<Value>,
        upper: Option<Value>,
    },
}
pub(super) fn ikey(tag: u8, id: IndexId) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend(ordered(id.0));
    k
}
pub(super) fn dkey(id: IndexId, copy: u8) -> Vec<u8> {
    let mut k = vec![DESCRIPTOR, copy];
    k.extend(ordered(id.0));
    k
}
pub(super) fn ckey(c: CollectionId, id: IndexId) -> Vec<u8> {
    let mut k = prefix(COLLECTION_INDEX, c);
    k.extend(ordered(id.0));
    k
}
pub(super) fn nkey(c: CollectionId, n: &str) -> Vec<u8> {
    let mut k = prefix(INDEX_NAME, c);
    k.extend(n.as_bytes());
    k
}
pub(crate) fn skey(i: &IndexInfo, value: &[u8], seq: u64) -> Vec<u8> {
    let mut k = Vec::new();
    skey_into(i, value, seq, &mut k);
    k
}
/// The same key, written into a buffer the caller reuses.
///
/// A late build derives one of these PER ROW and hands it straight to the
/// external sorter, which takes its own copy. Through `skey` that row paid for
/// four heap allocations it dropped a line later -- the one-byte `vec![tag]`
/// and its growth, plus a throwaway `Vec` from each `ordered` -- on top of the
/// one the sorter genuinely needs. Written into a scratch buffer they are all
/// gone after the first row.
pub(super) fn skey_into(i: &IndexInfo, value: &[u8], seq: u64, out: &mut Vec<u8>) {
    out.clear();
    out.push(SCALAR);
    super::ordered_into(out, i.id.0);
    out.extend_from_slice(value);
    super::ordered_into(out, seq);
}
fn kind_byte(k: &Kind) -> Result<u8> {
    match k {
        Kind::Bool => Ok(1),
        Kind::Int => Ok(2),
        Kind::Real => Ok(3),
        Kind::Text => Ok(4),
        _ => Err(invalid("scalar index requires bool/int/real/text")),
    }
}
/// The version-2 tail. Only the two families that own trees carry it, and a
/// descriptor's version and its `tree` field must agree in both directions:
/// version 1 with a tree, or version 2 without one, is a caller bug, not a
/// file this binary should write.
fn encode_tree(b: &mut Vec<u8>, i: &IndexInfo) -> Result<()> {
    match (i.encoding_version, i.tree) {
        (1, None) => Ok(()),
        (2 | 3, Some(t)) => {
            if t.id < 2 {
                return Err(invalid("per-index tree id 0/1 is reserved"));
            }
            b.extend(t.id.to_be_bytes());
            b.extend(t.root.to_be_bytes());
            Ok(())
        }
        _ => Err(invalid("index descriptor version does not match its tree")),
    }
}
pub(super) fn encode(i: &IndexInfo) -> Result<Vec<u8>> {
    if i.tree.is_some() && !matches!(i.family, IndexFamily::Scalar | IndexFamily::SpatialPoint) {
        return Err(invalid("only scalar and spatial indexes own a tree"));
    }
    if i.tree.is_none() && i.encoding_version != 1 {
        return Err(invalid("index descriptor version does not match its tree"));
    }
    // Version 3 is version 2 plus a one-byte expression tail, and only the
    // scalar family carries one: an expression index is a scalar index whose
    // value is derived.
    if (i.encoding_version == 3) != (i.expression.is_some())
        || (i.expression.is_some() && i.family != IndexFamily::Scalar)
    {
        return Err(invalid(
            "an expression index is a version-3 scalar descriptor and nothing else",
        ));
    }
    let mut b = i.id.0.to_be_bytes().to_vec();
    b.extend(i.collection.0.to_be_bytes());
    let (st, cursor) = match i.state {
        IndexState::Building { after } => (0, after),
        IndexState::Ready => (1, 0),
        IndexState::Dropping => (2, 0),
    };
    match i.family {
        IndexFamily::Scalar => {
            // This branch is the frozen scalar descriptor byte layout. Version
            // 2 appends `tree_id:u16be | root:u32be` after the cursor and
            // changes nothing before it, so the two versions share every key
            // encoding and differ only in which tree the keys live in.
            b.push(1);
            b.extend(i.encoding_version.to_be_bytes());
            b.push(kind_byte(&i.kind)?);
            b.push(u8::from(i.unique));
            b.push(st);
            b.extend(cursor.to_be_bytes());
            encode_tree(&mut b, i)?;
        }
        IndexFamily::ExactVector => {
            let Kind::Vector(dimension) = i.kind else {
                return Err(invalid("exact vector index requires a vector field"));
            };
            if !(1..=crate::index::vector::exact::MAX_DIM).contains(&dimension) || i.unique {
                return Err(invalid("invalid exact vector descriptor options"));
            }
            b.push(2);
            b.extend(i.encoding_version.to_be_bytes());
            b.extend(u32::try_from(dimension).map_err(invalid)?.to_be_bytes());
            b.push(0); // reserved vector options
            b.push(st);
            b.extend(cursor.to_be_bytes());
        }
        IndexFamily::SpatialGeometry => {
            if i.kind != Kind::Geo || i.unique {
                return Err(invalid(
                    "spatial geometry index requires a non-unique Geo field",
                ));
            }
            b.push(6);
            b.extend(i.encoding_version.to_be_bytes());
            b.push(crate::index::spatial::geometry_index::LEVEL_FINE);
            b.push(crate::index::spatial::geometry_index::LEVEL_COARSE);
            b.push(crate::index::spatial::geometry_index::LEVEL_WORLD);
            b.push(crate::index::spatial::geometry_index::MAX_CELLS);
            b.push(0); // reserved geometry options
            b.push(st);
            b.extend(cursor.to_be_bytes());
        }
        IndexFamily::SpatialPoint => {
            if i.kind != Kind::Point || i.unique {
                return Err(invalid(
                    "spatial point index requires a non-unique Point field",
                ));
            }
            b.push(3);
            b.extend(i.encoding_version.to_be_bytes());
            b.push(crate::index::spatial::point::GRID_BITS);
            b.push(crate::index::spatial::point::CRS_WGS84);
            b.push(crate::index::spatial::point::METRIC_KARNEY_V1);
            b.push(0); // reserved spatial options
            b.push(st);
            b.extend(cursor.to_be_bytes());
            encode_tree(&mut b, i)?;
        }
        IndexFamily::Text => {
            if i.kind != Kind::Text || i.unique {
                return Err(invalid("text index requires a non-unique Text field"));
            }
            b.push(4);
            b.extend(i.encoding_version.to_be_bytes());
            b.extend(crate::text_analyzer::ANALYZER_VERSION.to_be_bytes());
            let unicode = crate::text_analyzer::UNICODE_VERSION;
            b.extend([unicode.0, unicode.1, unicode.2]);
            b.extend(crate::index::text::BM25_VERSION.to_be_bytes());
            b.push(0); // reserved text options
            b.push(st);
            b.extend(cursor.to_be_bytes());
        }
        IndexFamily::QuantizedVector => {
            let Kind::Vector(dimension) = i.kind else {
                return Err(invalid("quantized vector index requires a vector field"));
            };
            if !(1..=crate::vector_quant::MAX_DIMENSION).contains(&dimension) || i.unique {
                return Err(invalid("invalid quantized vector descriptor options"));
            }
            b.push(5);
            b.extend(i.encoding_version.to_be_bytes());
            b.extend(u32::try_from(dimension).map_err(invalid)?.to_be_bytes());
            b.push(crate::index::vector::quantized::QUANTIZER_VERSION);
            b.push(crate::index::vector::quantized::OPTIONS);
            b.push(st);
            b.extend(cursor.to_be_bytes());
        }
    }
    for s in [&i.name, &i.field] {
        b.extend((s.len() as u16).to_be_bytes());
        b.extend(s.as_bytes());
    }
    if let Some(expression) = i.expression {
        b.push(expression.byte());
    }
    packet(MAGIC, &b)
}
pub(super) fn decode(b: &[u8]) -> Result<IndexInfo> {
    if b.len() == PAD && b.starts_with(b"E4IDX") && &b[..8] != MAGIC {
        unpack(b, b[..8].try_into().unwrap())?;
        return Err(Error::Unsupported("index descriptor envelope".into()));
    }
    let b = unpack(b, MAGIC)?;
    if b.len() < 15 {
        return Err(corrupt("short index descriptor"));
    }
    let id = IndexId(u64::from_be_bytes(b[..8].try_into().unwrap()));
    let collection = CollectionId(u32::from_be_bytes(b[8..12].try_into().unwrap()));
    let version = u16::from_be_bytes(b[13..15].try_into().unwrap());
    let family = match (b[12], version) {
        (1, 1 | 2 | 3) => IndexFamily::Scalar,
        (2, 1) => IndexFamily::ExactVector,
        (3, 1 | 2) => IndexFamily::SpatialPoint,
        (4, 1) => IndexFamily::Text,
        (5, 1) => IndexFamily::QuantizedVector,
        (6, 1) => IndexFamily::SpatialGeometry,
        _ => {
            return Err(Error::Unsupported(format!(
                "index family {} encoding {version}",
                b[12]
            )));
        }
    };
    let (kind, unique, state_at, cursor_at, mut at) = match family {
        IndexFamily::Scalar => {
            if b.len() < 26 {
                return Err(corrupt("short scalar index descriptor"));
            }
            let kind = match b[15] {
                1 => Kind::Bool,
                2 => Kind::Int,
                3 => Kind::Real,
                4 => Kind::Text,
                _ => return Err(Error::Unsupported("index scalar type".into())),
            };
            if b[16] > 1 {
                return Err(Error::Unsupported("index uniqueness option".into()));
            }
            (kind, b[16] != 0, 17, 18, 26)
        }
        IndexFamily::ExactVector => {
            if b.len() < 29 {
                return Err(corrupt("short exact vector index descriptor"));
            }
            let dimension = u32::from_be_bytes(b[15..19].try_into().unwrap()) as usize;
            if !(1..=crate::index::vector::exact::MAX_DIM).contains(&dimension) {
                return Err(corrupt("exact vector index dimension"));
            }
            if b[19] != 0 {
                return Err(Error::Unsupported("exact vector index options".into()));
            }
            (Kind::Vector(dimension), false, 20, 21, 29)
        }
        IndexFamily::SpatialGeometry => {
            if b.len() < 29 {
                return Err(corrupt("short spatial geometry index descriptor"));
            }
            if b[15] != crate::index::spatial::geometry_index::LEVEL_FINE
                || b[16] != crate::index::spatial::geometry_index::LEVEL_COARSE
                || b[17] != crate::index::spatial::geometry_index::LEVEL_WORLD
                || b[18] != crate::index::spatial::geometry_index::MAX_CELLS
                || b[19] != 0
            {
                return Err(Error::Unsupported(
                    "spatial geometry ladder/cell-budget/options".into(),
                ));
            }
            (Kind::Geo, false, 20, 21, 29)
        }
        IndexFamily::SpatialPoint => {
            if b.len() < 28 {
                return Err(corrupt("short spatial point index descriptor"));
            }
            if b[15] != crate::index::spatial::point::GRID_BITS
                || b[16] != crate::index::spatial::point::CRS_WGS84
                || b[17] != crate::index::spatial::point::METRIC_KARNEY_V1
                || b[18] != 0
            {
                return Err(Error::Unsupported(
                    "spatial point index grid/CRS/metric/options".into(),
                ));
            }
            (Kind::Point, false, 19, 20, 28)
        }
        IndexFamily::Text => {
            if b.len() < 32 {
                return Err(corrupt("short text index descriptor"));
            }
            if u16::from_be_bytes(b[15..17].try_into().unwrap())
                != crate::text_analyzer::ANALYZER_VERSION
                || b[17..20]
                    != [
                        crate::text_analyzer::UNICODE_VERSION.0,
                        crate::text_analyzer::UNICODE_VERSION.1,
                        crate::text_analyzer::UNICODE_VERSION.2,
                    ]
                || u16::from_be_bytes(b[20..22].try_into().unwrap())
                    != crate::index::text::BM25_VERSION
                || b[22] != 0
            {
                return Err(Error::Unsupported(
                    "text analyzer/Unicode/BM25/options".into(),
                ));
            }
            (Kind::Text, false, 23, 24, 32)
        }
        IndexFamily::QuantizedVector => {
            if b.len() < 30 {
                return Err(corrupt("short quantized vector index descriptor"));
            }
            let dimension = u32::from_be_bytes(b[15..19].try_into().unwrap()) as usize;
            if !(1..=crate::vector_quant::MAX_DIMENSION).contains(&dimension) {
                return Err(corrupt("quantized vector index dimension"));
            }
            if b[19] != crate::index::vector::quantized::QUANTIZER_VERSION || b[20] != 0 {
                return Err(Error::Unsupported(
                    "quantized vector quantizer/options".into(),
                ));
            }
            (Kind::Vector(dimension), false, 21, 22, 30)
        }
    };
    let tree = if version == 2 || version == 3 {
        let tail = b
            .get(at..at + 6)
            .ok_or_else(|| corrupt("short per-index tree descriptor tail"))?;
        at += 6;
        let id = u16::from_be_bytes(tail[..2].try_into().unwrap());
        if id < 2 {
            return Err(corrupt("per-index tree id 0/1 is reserved"));
        }
        Some(IndexTree {
            id,
            root: u32::from_be_bytes(tail[2..].try_into().unwrap()),
        })
    } else {
        None
    };
    let cursor = u64::from_be_bytes(b[cursor_at..cursor_at + 8].try_into().unwrap());
    let state = match b[state_at] {
        0 => IndexState::Building { after: cursor },
        1 if cursor == 0 => IndexState::Ready,
        2 if cursor == 0 => IndexState::Dropping,
        _ => return Err(Error::Unsupported("index lifecycle state/cursor".into())),
    };
    let mut string = || -> Result<String> {
        let size = b
            .get(at..at + 2)
            .ok_or_else(|| corrupt("index string size"))?;
        at += 2;
        let n = u16::from_be_bytes(size.try_into().unwrap()) as usize;
        if n == 0 || n > 128 {
            return Err(corrupt("index name/field length"));
        }
        let bytes = b
            .get(at..at + n)
            .ok_or_else(|| corrupt("index string bytes"))?;
        at += n;
        Ok(std::str::from_utf8(bytes).map_err(corrupt)?.to_owned())
    };
    let name = string()?;
    let field = string()?;
    let expression = if version == 3 {
        let byte = *b.get(at).ok_or_else(|| corrupt("index expression tail"))?;
        at += 1;
        Some(
            IndexExpr::from_byte(byte)?
                .ok_or_else(|| corrupt("version-3 index descriptor without an expression"))?,
        )
    } else {
        None
    };
    if at != b.len() || id.0 == 0 || collection.0 == 0 {
        return Err(corrupt("index descriptor identity/trailing bytes"));
    }
    Ok(IndexInfo {
        id,
        collection,
        name,
        field,
        family,
        kind,
        unique,
        state,
        encoding_version: version,
        tree,
        expression,
    })
}
pub(super) fn read_index(
    get: impl FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
    id: IndexId,
) -> Result<IndexInfo> {
    replicas(
        get,
        |copy| dkey(id, copy),
        |b| {
            let i = decode(b)?;
            if i.id != id {
                return Err(corrupt("index descriptor identity"));
            }
            Ok(i)
        },
    )
}
/// Called inside writer/snapshot admission, before any WAL normalization.
/// Streams bounded descriptors, never indexes or entities, with no resident registry.
pub(super) fn validate_catalog(s: &PageWalStore, h: Option<IndexHeader>) -> Result<()> {
    let mut count = 0u32;
    for row in s.range(&[REGISTRY])? {
        let (k, v) = row?;
        if k.first() != Some(&REGISTRY) {
            break;
        }
        let h = h.ok_or_else(|| corrupt("index registry without feature envelope"))?;
        let mut at = 1;
        let id = IndexId(read_ordered(&k, &mut at)?);
        if at != k.len() || id.0 == 0 || id.0 >= h.next || v.len() != 4 {
            return Err(corrupt("index registry entry"));
        }
        let i = read_index(|k| s.get(k).map_err(Error::from), id)?;
        if i.family == IndexFamily::ExactVector
            && h.features & crate::index::vector::exact::VECTOR_FEATURE == 0
        {
            return Err(corrupt("exact vector descriptor without feature admission"));
        }
        if i.family == IndexFamily::SpatialPoint
            && h.features & crate::index::spatial::point::SPATIAL_FEATURE == 0
        {
            return Err(corrupt(
                "spatial point descriptor without feature admission",
            ));
        }
        if i.family == IndexFamily::Text && h.features & crate::index::text::TEXT_FEATURE == 0 {
            return Err(corrupt("text descriptor without feature admission"));
        }
        if i.family == IndexFamily::QuantizedVector
            && h.features & crate::index::vector::quantized::QUANTIZED_VECTOR_FEATURE == 0
        {
            return Err(corrupt(
                "quantized vector descriptor without feature admission",
            ));
        }
        if i.family == IndexFamily::SpatialGeometry
            && h.features & crate::index::spatial::geometry_index::GEOMETRY_FEATURE == 0
        {
            return Err(corrupt(
                "spatial geometry descriptor without feature admission",
            ));
        }
        if i.expression.is_some() && h.features & EXPRESSION_FEATURE == 0 {
            return Err(corrupt(
                "expression index descriptor without feature admission",
            ));
        }
        if i.tree.is_some() && h.features & INDEX_TREE_FEATURE == 0 {
            return Err(corrupt(
                "per-index tree descriptor without feature admission",
            ));
        }
        if v != i.collection.0.to_be_bytes() {
            return Err(corrupt("index registry collection"));
        }
        if s.get(&ckey(i.collection, id))? != Some(vec![])
            || s.get(&nkey(i.collection, &i.name))? != Some(ordered(id.0))
        {
            return Err(corrupt("index catalog mapping"));
        }
        let c = replicas(
            |k| s.get(k).map_err(Error::from),
            |copy| replica_key(1, i.collection.0, copy),
            parse_catalog,
        )?;
        if c.id != i.collection {
            return Err(corrupt("indexed collection identity"));
        }
        let l = replicas(
            |k| s.get(k).map_err(Error::from),
            |copy| layout_key(c.layout, copy),
            |b| Layout::from_descriptor(b).map_err(corrupt),
        )?;
        if l.id != u64::from(c.layout)
            || !l.fields.iter().any(|(n, k)| n == &i.field && k == &i.kind)
        {
            return Err(corrupt("index field/layout mismatch"));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| corrupt("index count overflow"))?;
    }
    if count != h.map_or(0, |h| h.count) {
        return Err(corrupt("index registry count"));
    }
    // Check all descriptor keys, including orphans, so a registry omission
    // cannot conceal a newer required encoding from writer admission.
    for row in s.range(&[DESCRIPTOR])? {
        let (k, v) = row?;
        if k.first() != Some(&DESCRIPTOR) {
            break;
        }
        if k.len() < 3 || k[1] > 2 {
            return Err(corrupt("index replica key"));
        }
        let mut at = 2;
        let id = IndexId(read_ordered(&k, &mut at)?);
        if at != k.len() || id.0 == 0 {
            return Err(corrupt("index replica identity"));
        }
        if let Err(e @ Error::Unsupported(_)) = decode(&v) {
            return Err(e);
        }
        if s.get(&ikey(REGISTRY, id))?.is_none() {
            return Err(corrupt("orphan index descriptor"));
        }
        // Registered descriptors were already validated by independent copies.
    }
    let mut mapping_count = 0u32;
    let mut last_collection = None;
    let mut collection_count = 0usize;
    for row in s.range(&[COLLECTION_INDEX])? {
        let (k, v) = row?;
        if k.first() != Some(&COLLECTION_INDEX) {
            break;
        }
        let mut at = 1;
        let c = CollectionId(u32::try_from(read_ordered(&k, &mut at)?).map_err(corrupt)?);
        let id = IndexId(read_ordered(&k, &mut at)?);
        if at != k.len()
            || !v.is_empty()
            || s.get(&ikey(REGISTRY, id))? != Some(c.0.to_be_bytes().to_vec())
        {
            return Err(corrupt("orphan collection index mapping"));
        }
        if last_collection != Some(c) {
            last_collection = Some(c);
            collection_count = 0;
        }
        collection_count += 1;
        if collection_count > MAX_INDEXES {
            return Err(corrupt("collection index count exceeds 64"));
        }
        mapping_count = mapping_count
            .checked_add(1)
            .ok_or_else(|| corrupt("index mapping count overflow"))?;
    }
    if mapping_count != count {
        return Err(corrupt("collection index mapping count"));
    }
    let mut name_count = 0u32;
    for row in s.range(&[INDEX_NAME])? {
        let (k, v) = row?;
        if k.first() != Some(&INDEX_NAME) {
            break;
        }
        let mut at = 1;
        let c = CollectionId(u32::try_from(read_ordered(&k, &mut at)?).map_err(corrupt)?);
        let name = std::str::from_utf8(&k[at..]).map_err(corrupt)?;
        let mut pos = 0;
        let id = IndexId(read_ordered(&v, &mut pos)?);
        if pos != v.len() || s.get(&ikey(REGISTRY, id))?.is_none() {
            return Err(corrupt("orphan index name"));
        }
        let i = read_index(|k| s.get(k).map_err(Error::from), id)?;
        if i.collection != c || i.name != name {
            return Err(corrupt("index name mapping mismatch"));
        }
        name_count = name_count
            .checked_add(1)
            .ok_or_else(|| corrupt("index name count overflow"))?;
    }
    if name_count != count {
        return Err(corrupt("index name count"));
    }
    if !h.is_some_and(|header| header.features & crate::index::vector::exact::VECTOR_FEATURE != 0) {
        if let Some(row) = s.range(&[crate::index::vector::exact::VECTOR_ENTRY])?.next() {
            let (key, _) = row?;
            if key.first() == Some(&crate::index::vector::exact::VECTOR_ENTRY) {
                return Err(corrupt("exact vector entries without feature admission"));
            }
        }
    }
    if !h.is_some_and(|header| header.features & crate::index::spatial::point::SPATIAL_FEATURE != 0) {
        if let Some(row) = s.range(&[crate::index::spatial::point::POINT_ENTRY])?.next() {
            let (key, _) = row?;
            if key.first() == Some(&crate::index::spatial::point::POINT_ENTRY) {
                return Err(corrupt("spatial point entries without feature admission"));
            }
        }
    }
    if !h.is_some_and(|header| header.features & crate::index::text::TEXT_FEATURE != 0) {
        for tag in crate::index::text::TEXT_TAGS {
            if let Some(row) = s.range(&[tag])?.next() {
                let (key, _) = row?;
                if key.first() == Some(&tag) {
                    return Err(corrupt("text entries without feature admission"));
                }
            }
        }
    }
    if !h.is_some_and(|header| {
        header.features & crate::index::text::segments::SEGMENT_FEATURE != 0
    }) {
        for tag in [
            crate::index::text::segments::SEGMENT,
            crate::index::text::segments::NORM_BLOCK,
        ] {
            if let Some(row) = s.range(&[tag])?.next() {
                let (key, _) = row?;
                if key.first() == Some(&tag) {
                    return Err(corrupt("text segments without feature admission"));
                }
            }
        }
    }
    if !h.is_some_and(|header| {
        header.features & crate::index::vector::quantized::QUANTIZED_VECTOR_FEATURE != 0
    }) {
        if let Some(row) = s
            .range(&[crate::index::vector::quantized::QUANTIZED_VECTOR_ENTRY])?
            .next()
        {
            let (key, _) = row?;
            if key.first() == Some(&crate::index::vector::quantized::QUANTIZED_VECTOR_ENTRY) {
                return Err(corrupt(
                    "quantized vector entries without feature admission",
                ));
            }
        }
    }
    if !h.is_some_and(|header| header.features & crate::index::spatial::geometry_index::GEOMETRY_FEATURE != 0) {
        if let Some(row) = s.range(&[crate::index::spatial::geometry_index::GEOM_ENTRY])?.next() {
            let (key, _) = row?;
            if key.first() == Some(&crate::index::spatial::geometry_index::GEOM_ENTRY) {
                return Err(corrupt("spatial geometry entries without feature admission"));
            }
        }
    }
    Ok(())
}
impl Database {
    /// Ascending entries of one index from `from`.
    ///
    /// A version-1 index's entries are records of the primary tree and this is
    /// the ordinary scan the code has always done. A version-2 index's entries
    /// are records of ITS tree, reached through the root its descriptor holds.
    /// Key encodings are identical in both layouts -- the family tag is still
    /// the first byte -- so every caller's prefix test, decode and stop
    /// condition is unchanged; only the tree the cursor walks differs.
    /// `None` is an index whose tree is still empty.
    pub(crate) fn index_range(
        &self,
        i: &IndexInfo,
        from: &[u8],
    ) -> Result<Option<kernel::btree::RangeIter<'_>>> {
        match i.tree {
            None => self.store()?.range(from).map(Some).map_err(Error::from),
            Some(t) => self.store()?.tree_range(t.id, t.root, from).map_err(Error::from),
        }
    }
    /// Descending entries of one index, strictly below `to`. The mirror of
    /// [`Database::index_range`], and the same two layouts: a version-1 index
    /// walks the primary tree backwards, a version-2 index walks its own.
    pub(crate) fn index_range_reverse(
        &self,
        i: &IndexInfo,
        to: &[u8],
    ) -> Result<Option<kernel::btree::ReverseRangeIter<'_>>> {
        match i.tree {
            None => self.store()?.range_reverse(to).map(Some).map_err(Error::from),
            Some(t) => self
                .store()?
                .tree_range_reverse(t.id, t.root, to)
                .map_err(Error::from),
        }
    }
    /// One entry of one index.
    pub(crate) fn index_get(&self, i: &IndexInfo, k: &[u8]) -> Result<Option<Vec<u8>>> {
        match i.tree {
            None => self.store()?.get(k).map_err(Error::from),
            Some(t) => self.store()?.tree_get(t.id, t.root, k).map_err(Error::from),
        }
    }
    /// Write one entry of one index.
    ///
    /// The root of a per-index tree moves when the tree grows a level, and the
    /// only durable copy of that root is the descriptor. So the descriptor is
    /// rewritten HERE, in the same transaction as the page split that moved
    /// it: both are frames of one commit, and there is no window in which a
    /// crash could leave a committed tree whose committed descriptor points
    /// somewhere else. `i` is updated in place so a caller writing several
    /// entries does not re-read it.
    pub(crate) fn index_put(&mut self, i: &mut IndexInfo, k: &[u8], v: &[u8]) -> Result<()> {
        let Some(t) = i.tree else {
            return self.writer()?.put(k, v).map_err(Error::from);
        };
        let root = if t.root == 0 {
            self.writer()?.tree_create(t.id)?
        } else {
            t.root
        };
        let after = self.writer()?.tree_put(t.id, root, k, v)?;
        if after != t.root {
            i.tree = Some(IndexTree { id: t.id, root: after });
            self.save_index(i)?;
        }
        Ok(())
    }
    /// Remove one entry of one index, rewriting the descriptor in the same
    /// transaction if the delete collapsed a level.
    pub(crate) fn index_delete(&mut self, i: &mut IndexInfo, k: &[u8]) -> Result<bool> {
        let Some(t) = i.tree else {
            return self.writer()?.delete(k).map_err(Error::from);
        };
        let (found, after) = self.writer()?.tree_delete(t.id, t.root, k)?;
        if after != t.root {
            i.tree = Some(IndexTree { id: t.id, root: after });
            self.save_index(i)?;
        }
        Ok(found)
    }
    pub fn index_info(&self, id: IndexId) -> Result<IndexInfo> {
        let s = self.store()?;
        if s.get(&ikey(REGISTRY, id))?.is_none() {
            return Err(Error::NotFound("index"));
        }
        read_index(|k| s.get(k).map_err(Error::from), id)
    }
    pub fn list_indexes(&self, c: CollectionId) -> Result<Vec<IndexInfo>> {
        self.catalog(c)?;
        self.list_indexes_any(c)
    }
    /// The same registry walk without the live-collection check. Only the
    /// drop path uses it: a DROPPING collection still owns indexes, and
    /// removing them is the first phase of removing it.
    pub(super) fn list_indexes_any(&self, c: CollectionId) -> Result<Vec<IndexInfo>> {
        if self.index_header.is_none() {
            return Ok(vec![]);
        }
        let p = prefix(COLLECTION_INDEX, c);
        let mut out = Vec::new();
        for row in self.store()?.range(&p)? {
            let (k, v) = row?;
            if !k.starts_with(&p) {
                break;
            }
            let mut at = p.len();
            let id = IndexId(read_ordered(&k, &mut at)?);
            if at != k.len() || !v.is_empty() || out.len() >= MAX_INDEXES {
                return Err(corrupt("collection index registry"));
            }
            let i = self.index_info(id)?;
            if i.collection != c {
                return Err(corrupt("index collection mismatch"));
            }
            out.push(i);
        }
        Ok(out)
    }
    pub(crate) fn save_index(&mut self, i: &IndexInfo) -> Result<()> {
        let b = encode(i)?;
        // Every descriptor write in a live handle passes here, so this is
        // where the write-path descriptor list stops being believable.
        self.index_descriptors_changed();
        for copy in 0..3 {
            self.writer()?.put(&dkey(i.id, copy), &b)?;
        }
        Ok(())
    }
    /// Explicit opt-in. On populated collections, callers advance bounded build
    /// steps and commit them. Ordinary writes maintain BUILDING and READY indexes.
    pub fn create_scalar_index(
        &mut self,
        c: CollectionId,
        name: &str,
        field: &str,
        unique: bool,
    ) -> Result<IndexId> {
        self.ready_write()?;
        if name.is_empty() || name.len() > 128 || field.is_empty() || field.len() > 128 {
            return Err(invalid("index name and field require 1..128 UTF-8 bytes"));
        }
        let info = self.collection_info(c)?;
        let kind = info
            .layout
            .fields
            .iter()
            .find(|(n, _)| n == field)
            .map(|(_, k)| k.clone())
            .ok_or_else(|| invalid("index field must be declared"))?;
        kind_byte(&kind)?;
        self.create_index(c, name, field, kind, unique, IndexFamily::Scalar, 0)
    }

    /// An EXPRESSION scalar index: the same family, the same keys and the
    /// same walk, over `expression(field)` rather than `field`.
    ///
    /// `docs/lang/QL_CONTRACT.md` §4.1: `lower(col) = x` and `lower(col) LIKE
    /// 'x%'` are answered index-side only when this index exists; without it
    /// they are REFUSED, never demoted to a scan. The cost of holding one is
    /// one derived value per write, which is the ordinary scalar maintenance
    /// plus the expression.
    pub fn create_expression_index(
        &mut self,
        c: CollectionId,
        name: &str,
        field: &str,
        expression: IndexExpr,
        unique: bool,
    ) -> Result<IndexId> {
        self.ready_write()?;
        if name.is_empty() || name.len() > 128 || field.is_empty() || field.len() > 128 {
            return Err(invalid("index name and field require 1..128 UTF-8 bytes"));
        }
        let info = self.collection_info(c)?;
        let kind = info
            .layout
            .fields
            .iter()
            .find(|(n, _)| n == field)
            .map(|(_, k)| k.clone())
            .ok_or_else(|| invalid("index field must be declared"))?;
        if !matches!(expression, IndexExpr::Lower) || kind != Kind::Text {
            return Err(invalid("lower(col) is an expression over a TEXT field"));
        }
        kind_byte(&kind)?;
        self.create_index_full(
            c,
            name,
            field,
            kind,
            unique,
            IndexFamily::Scalar,
            EXPRESSION_FEATURE,
            true,
            Some(expression),
        )
    }

    pub(crate) fn create_index(
        &mut self,
        c: CollectionId,
        name: &str,
        field: &str,
        kind: Kind,
        unique: bool,
        family: IndexFamily,
        feature: u64,
    ) -> Result<IndexId> {
        self.create_index_with_tree(
            c,
            name,
            field,
            kind,
            unique,
            family,
            feature,
            own_tree(family, self.create_index_trees),
        )
    }
    /// Tree ids are drawn from the SAME monotone counter as index identities
    /// (`header.next`), one above the index's own id, so tree 1 -- the primary
    /// tree -- is never handed out and a tree id is never reused: an index id
    /// is not reused either, and a dropped index's tree pages return to the
    /// freelist under a number nothing will claim again. That costs the u16
    /// space at the rate indexes are CREATED over a database's life, not at
    /// the rate they are held; creation past 65534 is refused rather than
    /// wrapped (SACRIFICE, Law 4).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_index_with_tree(
        &mut self,
        c: CollectionId,
        name: &str,
        field: &str,
        kind: Kind,
        unique: bool,
        family: IndexFamily,
        feature: u64,
        with_tree: bool,
    ) -> Result<IndexId> {
        self.create_index_full(c, name, field, kind, unique, family, feature, with_tree, None)
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_index_full(
        &mut self,
        c: CollectionId,
        name: &str,
        field: &str,
        kind: Kind,
        unique: bool,
        family: IndexFamily,
        feature: u64,
        with_tree: bool,
        expression: Option<IndexExpr>,
    ) -> Result<IndexId> {
        if name.is_empty() || name.len() > 128 || field.is_empty() || field.len() > 128 {
            return Err(invalid("index name and field require 1..128 UTF-8 bytes"));
        }
        if self.store()?.get(&nkey(c, name))?.is_some() {
            return Err(Error::AlreadyExists);
        }
        if self.list_indexes(c)?.len() >= MAX_INDEXES {
            return Err(invalid("at most 64 indexes per collection"));
        }
        let mut header = self.index_header.unwrap_or(IndexHeader {
            features: 1,
            next: 1,
            count: 0,
        });
        header.features |= feature;
        let id = IndexId(header.next);
        let tree = if with_tree {
            let tree_id = u16::try_from(id.0 + 1)
                .map_err(|_| invalid("per-index tree identities exhausted"))?;
            header.features |= INDEX_TREE_FEATURE;
            Some(IndexTree { id: tree_id, root: 0 })
        } else {
            None
        };
        header.next = header
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("index identities exhausted"))?;
        header.count = header
            .count
            .checked_add(1)
            .ok_or_else(|| invalid("index count exhausted"))?;
        let i = IndexInfo {
            id,
            collection: c,
            name: name.into(),
            field: field.into(),
            family,
            kind,
            unique,
            state: IndexState::Building { after: 0 },
            encoding_version: match (expression.is_some(), tree.is_some()) {
                (true, true) => 3,
                (false, true) => 2,
                // An expression index needs its own tree: the version that
                // carries the expression tail is the version that carries the
                // tree tail, so a handle with per-index trees turned off
                // cannot create one.
                (true, false) => {
                    return Err(invalid(
                        "an expression index requires a per-index tree (descriptor version 3)",
                    ))
                }
                (false, false) => 1,
            },
            tree,
            expression,
        };
        let (nc, nl) = self.header()?;
        let result = (|| {
            self.index_header = Some(header);
            self.write_header(nc, nl)?;
            self.save_index(&i)?;
            self.writer()?
                .put(&ikey(REGISTRY, id), &c.0.to_be_bytes())?;
            self.writer()?.put(&ckey(c, id), &[])?;
            self.writer()?.put(&nkey(c, name), &ordered(id.0))?;
            Ok(id)
        })();
        self.finish(result)
    }
    /// Turn on one logical feature bit in the collection header. Monotone:
    /// the bit is only ever set, never cleared, and only by an explicit
    /// creation or a build that actually writes the new representation --
    /// never by opening or by an ordinary write (Law 8).
    pub(crate) fn enable_index_feature(&mut self, feature: u64) -> Result<()> {
        let mut header = self
            .index_header
            .ok_or_else(|| corrupt("missing index header"))?;
        if header.features & feature == feature {
            return Ok(());
        }
        header.features |= feature;
        let (nc, nl) = self.header()?;
        self.index_header = Some(header);
        self.write_header(nc, nl)
    }
    /// [`Self::enable_index_feature`] for a bit that does not ride an index
    /// creation and so may be the first thing in a database to need the
    /// index-header envelope at all -- `DECLARED_FEATURE`, which rides a
    /// `CREATE TABLE`. The envelope is created rather than demanded, the same
    /// way `drop_collection.rs` creates it for `DROP_FEATURE`. Still monotone,
    /// and still written in the caller's own transaction.
    pub(crate) fn enable_logical_feature(&mut self, feature: u64) -> Result<()> {
        let mut header = self.index_header.unwrap_or(IndexHeader {
            features: 1,
            next: 1,
            count: 0,
        });
        if self.index_header.is_some() && header.features & feature == feature {
            return Ok(());
        }
        header.features |= feature;
        let (nc, nl) = self.header()?;
        self.index_header = Some(header);
        self.write_header(nc, nl)
    }
    pub(super) fn validate_indexed_layout(&self, c: CollectionId, l: &Layout) -> Result<()> {
        for i in self.list_indexes(c)? {
            if !l.fields.iter().any(|(n, k)| n == &i.field && k == &i.kind) {
                return Err(invalid(
                    "drop indexed field's indexes completely before removing/retyping it",
                ));
            }
        }
        Ok(())
    }
    fn check_unique(&self, i: &IndexInfo, value: &[u8], seq: u64) -> Result<()> {
        if !i.unique || value == [0] {
            return Ok(());
        }
        let mut p = ikey(SCALAR, i.id);
        p.extend(value);
        for row in self.index_range(i, &p)?.into_iter().flatten() {
            let (k, v) = row?;
            if !k.starts_with(&p) {
                break;
            }
            let mut at = p.len();
            let other = read_ordered(&k, &mut at)?;
            if at != k.len() || !v.is_empty() {
                return Err(corrupt("unique scalar entry"));
            }
            if other != seq {
                return Err(Error::AlreadyExists);
            }
        }
        Ok(())
    }
    pub(super) fn maintain_indexes(
        &mut self,
        id: EntityId,
        old: Option<&Value>,
        new: Option<&Value>,
        new_vectors: Option<(&Layout, &VectorCells)>,
    ) -> Result<()> {
        // The descriptors of one collection do not change when a row is
        // written, so a run of writes reads them ONCE. See
        // `Database::index_list_cache` for the generation that says when a
        // taken list has stopped being true.
        let generation = self.index_list_generation();
        let mut list = self.take_index_list(id.collection, generation)?;
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(
                list,
                self.list_indexes(id.collection)?,
                "the write-path index list cache is stale: a descriptor writer did not announce itself"
            );
        }
        // A row write is an INSERT when there is no old document: the entity
        // id was allocated for this write and has never been on disk, so no
        // index entry keyed by its sequence can exist. The families that
        // probe for one before writing it can skip that read.
        let fresh = old.is_none() && new.is_some();
        let result = (|| -> Result<()> {
        for i in list.iter_mut() {
            if i.state == IndexState::Dropping {
                continue;
            }
            if i.family == IndexFamily::ExactVector {
                crate::index::vector::exact::maintain_locator(self, i, id, new_vectors, fresh)?;
                continue;
            }
            if i.family == IndexFamily::QuantizedVector {
                crate::index::vector::quantized::maintain_entry(self, i, id, new_vectors, fresh)?;
                continue;
            }
            if i.family == IndexFamily::SpatialPoint {
                crate::index::spatial::point::maintain_point(self, i, id, old, new)?;
                continue;
            }
            if i.family == IndexFamily::SpatialGeometry {
                crate::index::spatial::geometry_index::maintain_geometry(self, i, id, old, new)?;
                continue;
            }
            if i.family == IndexFamily::Text {
                crate::index::text::maintain_text(self, i, id, old, new)?;
                continue;
            }
            // An expression index stores `expression(field)`. The derived
            // value is computed HERE, on the write path, from the field the
            // row already carries: no row byte changes and no second read
            // happens.
            let derived = |doc: &Value| -> Result<Vec<u8>> {
                match i.expression {
                    None => scalar_key::encode(&i.kind, doc.get(&i.field)),
                    Some(expression) => {
                        let value = expression.apply(doc.get(&i.field));
                        scalar_key::encode(&i.kind, value.as_ref())
                    }
                }
            };
            let a = old.map(&derived).transpose()?;
            let b = new.map(&derived).transpose()?;
            if a == b {
                continue;
            }
            if let Some(v) = &b {
                self.check_unique(i, v, id.sequence)?;
            }
            if let Some(v) = a {
                let k = skey(i, &v, id.sequence);
                self.index_delete(i, &k)?;
            }
            if let Some(v) = b {
                let k = skey(i, &v, id.sequence);
                self.index_put(i, &k, &[])?;
            }
        }
        Ok(())
        })();
        // The list goes back only if nothing wrote a descriptor while it was
        // out; `put_index_list` checks the generation itself.
        self.put_index_list(id.collection, generation, list);
        result
    }
    /// The scalar key of one immutable row, read without materializing the rest
    /// of the document.
    ///
    /// The build used to decode the whole row into a `serde_json::Map` --
    /// every string allocated, every vector sidecar fetched from the store --
    /// to then read a single field out of it. `read_field` walks the same
    /// dense-v3 row and keeps only the field asked for, which is what the text
    /// and spatial builders already do, and it never touches the vector
    /// keyspace.
    ///
    /// Equivalence: for any indexable field these are the same value.
    /// `read_field` resolves undeclared names out of the row's extras exactly
    /// as the decoded document would, `Missing` is `None` and `Null` is
    /// `Some(Null)` on both sides, and the external-key field is refused as a
    /// collection field name (`reserved`), so it can never be the index field.
    fn scalar_build_key(&self, i: &IndexInfo, row: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.scalar_build_key_into(i, row, &mut out)?;
        Ok(out)
    }
    /// The same value, encoded into a buffer the caller reuses. `encode_into`
    /// clears it; the caller keeps the capacity.
    fn scalar_build_key_into(&self, i: &IndexInfo, row: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let layout = self.layout(layout_id(row)?)?;
        let value = match crate::dense_v3::read_field(&layout, row, &i.field).map_err(corrupt)? {
            crate::dense_v3::FieldValue::Missing => None,
            crate::dense_v3::FieldValue::Null => Some(Value::Null),
            crate::dense_v3::FieldValue::Inline(v) => Some(v),
            crate::dense_v3::FieldValue::Vector { .. } => {
                return Err(invalid("historical indexed scalar field changed kind"))
            }
        };
        match i.expression {
            None => scalar_key::encode_into(&i.kind, value.as_ref(), out),
            Some(expression) => {
                let source = value;
                let value = expression.apply(source.as_ref());
                scalar_key::encode_into(&i.kind, value.as_ref(), out).map_err(|error| {
                    // NAMED refusal, because the ordinary one would be a lie
                    // about which value is too long. Unicode simple
                    // lowercasing can make a string LONGER in bytes -- `İ`
                    // (U+0130, 2 bytes) lowers to `i` + U+0307 (3 bytes) --
                    // so a source value inside the 1024-byte text key limit
                    // can have an image outside it. The row is written and
                    // the INDEX cannot hold it; there is no shorter key that
                    // is still the expression's value, so it is refused
                    // rather than truncated to a key that would answer
                    // `lower(col) = x` with the wrong rows (Law 8).
                    let source_bytes = source
                        .as_ref()
                        .and_then(Value::as_str)
                        .map_or(0, |s| s.len());
                    let image_bytes = value.as_ref().and_then(Value::as_str).map_or(0, |s| s.len());
                    if image_bytes > source_bytes {
                        invalid(format!(
                            "{}(col) of this value is {image_bytes} UTF-8 bytes where the value itself is {source_bytes}, and a scalar Text index key holds at most 1024: Unicode lowercasing can lengthen a string, so an expression index over lower(col) cannot accept every value the column can. Shorten the value or drop the expression index",
                            expression.written()
                        ))
                    } else {
                        error
                    }
                })
            }
        }
    }
    /// One transaction's bounded work. No implicit commit; true means READY in
    /// this transaction, visible to other readers only after caller commits.
    pub fn build_index_step(&mut self, id: IndexId, batch: usize) -> Result<bool> {
        enum BuiltEntry {
            Scalar(Vec<u8>),
            ExactVector(Option<[u8; 6]>),
            QuantizedVector(Option<Vec<u8>>),
            SpatialPoint(Option<crate::index::spatial::point::PointEntry>),
            SpatialGeometry(Vec<crate::index::spatial::geometry_index::GeometryEntry>),
            Text(Option<crate::text_analyzer::Analysis>),
        }
        self.ready_write()?;
        if !(1..=MAX_BATCH).contains(&batch) {
            return Err(invalid("index batch must be 1..256"));
        }
        let mut i = self.index_info(id)?;
        let after = match i.state {
            IndexState::Ready => return Ok(true),
            IndexState::Building { after } => after,
            IndexState::Dropping => return Err(invalid("index is dropping")),
        };
        // Capture at most 256 scalar keys or fixed-width vector locators.
        let mut rows = Vec::new();
        let mut last = after;
        let mut end = true;
        let start = if after == 0 {
            prefix(0x40, i.collection)
        } else {
            row_key(EntityId {
                collection: i.collection,
                sequence: after,
            })
        };
        let p = prefix(0x40, i.collection);
        for row in self.store()?.range(&start)? {
            let (key, value) = row?;
            if !key.starts_with(&p) {
                break;
            }
            let eid = row_id(&key)?;
            if eid.sequence <= after {
                continue;
            }
            if rows.len() == batch {
                end = false;
                break;
            }
            let entry = match i.family {
                IndexFamily::Scalar => BuiltEntry::Scalar(self.scalar_build_key(&i, &value)?),
                IndexFamily::ExactVector => BuiltEntry::ExactVector(
                    crate::index::vector::exact::build_locator(self, &i, eid, &value)?,
                ),
                IndexFamily::QuantizedVector => BuiltEntry::QuantizedVector(
                    crate::index::vector::quantized::build_entry(self, &i, eid, &value)?,
                ),
                IndexFamily::SpatialPoint => BuiltEntry::SpatialPoint(
                    crate::index::spatial::point::build_point_entry(self, &i, eid, &value)?,
                ),
                IndexFamily::SpatialGeometry => BuiltEntry::SpatialGeometry(
                    crate::index::spatial::geometry_index::build_geometry_entries(self, &i, eid, &value)?,
                ),
                IndexFamily::Text => {
                    BuiltEntry::Text(crate::index::text::analyze_row_bytes(self, &i, &value)?)
                }
            };
            rows.push((eid.sequence, entry));
            last = eid.sequence;
        }
        let result = (|| {
            // Rows arrive in entity-sequence order, which is a random order in
            // the scalar key space: every insert is then a blind descent into
            // the middle of the tree, and most of them split a leaf that is not
            // the rightmost one. Sorting the chunk by the key it is about to
            // write turns it into one ascending run, which is the shape the
            // kernel's per-keyspace append path is built for. Across chunks a
            // small number of leaves are revisited; inside one, none are.
            //
            // The sort keeps equal values adjacent, so `check_unique` still
            // sees an earlier duplicate from this same chunk and still refuses
            // it -- only which of the two rows reports the violation can move.
            if i.family == IndexFamily::Scalar {
                rows.sort_by(|a, b| match (&a.1, &b.1) {
                    (BuiltEntry::Scalar(x), BuiltEntry::Scalar(y)) => (x, a.0).cmp(&(y, b.0)),
                    _ => std::cmp::Ordering::Equal,
                });
            }
            if i.family == IndexFamily::Text {
                let documents = std::mem::take(&mut rows)
                    .into_iter()
                    .map(|(seq, entry)| {
                        let BuiltEntry::Text(analysis) = entry else {
                            unreachable!()
                        };
                        (
                            EntityId {
                                collection: i.collection,
                                sequence: seq,
                            },
                            analysis,
                        )
                    })
                    .collect();
                crate::index::text::build_documents(self, &i, documents)?;
            }
            for (seq, entry) in rows {
                match entry {
                    BuiltEntry::Scalar(v) => {
                        self.check_unique(&i, &v, seq)?;
                        let k = skey(&i, &v, seq);
                        self.index_put(&mut i, &k, &[])?;
                    }
                    BuiltEntry::ExactVector(Some(locator)) => self
                        .writer()?
                        .put(&crate::index::vector::exact::locator_key(i.id, seq), &locator)?,
                    BuiltEntry::ExactVector(None) => {}
                    BuiltEntry::QuantizedVector(Some(value)) => self.writer()?.put(
                        &crate::index::vector::quantized::entry_key(i.id, seq),
                        &value,
                    )?,
                    BuiltEntry::QuantizedVector(None) => {}
                    BuiltEntry::SpatialPoint(Some(point)) => {
                        self.index_put(&mut i, &point.key, &point.value)?
                    }
                    BuiltEntry::SpatialPoint(None) => {}
                    BuiltEntry::SpatialGeometry(entries) => {
                        for entry in entries {
                            self.index_put(&mut i, &entry.key, &entry.value)?;
                        }
                    }
                    // Text is written above, one whole chunk at a time.
                    BuiltEntry::Text(_) => unreachable!(),
                }
            }
            i.state = if end {
                IndexState::Ready
            } else {
                IndexState::Building { after: last }
            };
            self.save_index(&i)?;
            Ok(end)
        })();
        self.finish(result)
    }
    pub(super) fn sort_scratch(&self) -> std::path::PathBuf {
        // Spill under TMPDIR (tests pin it) in a per-database subdirectory.
        let mut dir = std::env::temp_dir();
        dir.push("e4-index-sort");
        dir.push(self.path.file_name().unwrap_or_default());
        dir
    }
    pub(super) fn scan_collection_rows(
        &self,
        collection: CollectionId,
        mut f: impl FnMut(EntityId, &[u8]) -> Result<()>,
    ) -> Result<u64> {
        let p = prefix(0x40, collection);
        let mut last = 0u64;
        // `for_each_ref` hands the callback borrows into the pinned leaf. The
        // allocating iterator built a `Vec` for the key and a `Vec` for the
        // whole row -- two per document, the row one as large as the document
        // -- to hand a builder bytes it only reads. Every other scan in this
        // file already takes the borrowed form; the primary-row scan every
        // late build starts from was the one that did not.
        let mut failure: Option<Error> = None;
        self.store()?.range(&p)?.for_each_ref(|key, value| {
            if !key.starts_with(&p) {
                return false;
            }
            match row_id(key).and_then(|eid| {
                f(eid, value)?;
                Ok(eid.sequence)
            }) {
                Ok(sequence) => {
                    last = sequence;
                    true
                }
                Err(error) => {
                    failure = Some(error);
                    false
                }
            }
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(last)
    }
    /// `scan_collection_rows`, bounded: at most `limit` rows after `after`.
    ///
    /// A whole-collection scan hands the callback a borrow of the store, so a
    /// builder that wants to WRITE what it just read has to buffer everything
    /// until the scan is over -- RAM proportional to the store. Scanning in
    /// bounded runs gives the builder a place to stand between runs where the
    /// store is not borrowed and it can flush. Returns the last sequence
    /// visited and whether the collection is exhausted.
    pub(crate) fn scan_collection_rows_from(
        &self,
        collection: CollectionId,
        after: u64,
        limit: usize,
        mut f: impl FnMut(EntityId, &[u8]) -> Result<()>,
    ) -> Result<(u64, bool)> {
        let p = prefix(0x40, collection);
        let start = if after == 0 {
            p.clone()
        } else {
            row_key(EntityId {
                collection,
                sequence: after,
            })
        };
        let mut last = after;
        let mut seen = 0usize;
        let mut exhausted = true;
        let mut failure: Option<Error> = None;
        self.store()?.range(&start)?.for_each_ref(|key, value| {
            if !key.starts_with(&p) {
                return false;
            }
            let eid = match row_id(key) {
                Ok(eid) => eid,
                Err(error) => {
                    failure = Some(error);
                    return false;
                }
            };
            if eid.sequence <= after {
                return true;
            }
            if seen == limit {
                exhausted = false;
                return false;
            }
            if let Err(error) = f(eid, value) {
                failure = Some(error);
                return false;
            }
            seen += 1;
            last = eid.sequence;
            true
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok((last, exhausted))
    }
    fn scalar_value_bytes<'a>(&self, i: &IndexInfo, key: &'a [u8]) -> Result<&'a [u8]> {
        let p = ikey(SCALAR, i.id);
        if !key.starts_with(&p) {
            return Err(corrupt("sorted scalar key prefix"));
        }
        let (_, n) = scalar_key::decode(&i.kind, &key[p.len()..])?;
        Ok(&key[p.len()..p.len() + n])
    }
    /// Entry bytes one transaction of a sorted build may carry.
    ///
    /// The page-WAL logs one 4144-byte FRAME per page a transaction touches
    /// and refuses the transaction WHOLE once those frames pass the managed
    /// allowance -- a fixed 16 MiB, or a smaller installed `wal_bytes`. A
    /// build that must finish at any size therefore has to pick its own
    /// transaction size. Packing a whole index in one transaction picks it by
    /// accident, and past roughly half a million scalar rows the accident is a
    /// refusal.
    ///
    /// The arithmetic, for an allowance A:
    ///
    /// ```text
    ///   budget  = A / 4                      bytes one transaction may spend
    ///   frames  = budget / 4144              pages those bytes carry
    ///   leaves  = frames * 3/4 - 4           pages for LEAF content
    ///   entries = leaves * (4096 - 40) * 0.9 entry bytes those leaves hold
    /// ```
    ///
    /// A QUARTER of the allowance, not all of it, because the WAL is only
    /// folded BETWEEN transactions: `fold_committed_wal_if_at_cap` runs at the
    /// first write of a transaction and checkpoints once the WAL has reached
    /// `min(4 MiB, wal_bytes/2)`. So at most one un-folded transaction can
    /// still be standing when the next one starts, and a quarter keeps the two
    /// of them together inside A whichever of those thresholds applies. (At
    /// the unlimited 16 MiB the two numbers coincide exactly: a quarter of the
    /// allowance IS the 4 MiB fold threshold.)
    ///
    /// THREE QUARTERS of the frames for leaves, less four, because a
    /// transaction logs more than leaf content: the pack's own interior levels
    /// (far smaller -- a 4 KiB interior page holds many more separators than a
    /// leaf holds entries, so the real share is nearer one page in a hundred),
    /// the collection header page, the index descriptor's three replicas and
    /// the commit frame. The flat four covers the fixed pages at small
    /// allowances, where a fraction alone would not.
    ///
    /// The estimate does not have to be right, only conservative: a refusal
    /// still rolls back, halves the group and carries on. Being conservative
    /// is what stops that from being the normal path.
    fn sorted_run_budget(&self) -> Result<u64> {
        /// A logged page and its frame header, `FRAME` in `src/pagewal.rs`.
        const WAL_FRAME_BYTES: u64 = 4096 + 48;
        /// What a leaf packed at 0.9 holds: `(PAGE_SIZE - HEADER_LEN) * fill`.
        const PACKED_LEAF_BYTES: u64 = ((4096 - 40) * 9) / 10;
        let frames = self.store()?.wal_allowance() / 4 / WAL_FRAME_BYTES;
        let leaves = (frames * 3 / 4).saturating_sub(4).max(1);
        Ok(leaves * PACKED_LEAF_BYTES)
    }
    /// Drive a late build to READY in bounded transactions, committing each
    /// chunk while the descriptor still says BUILDING.
    ///
    /// Atomic *publication* is about what a reader can see, not about how many
    /// transactions the build takes. `query_scalar` and every other family's
    /// query refuse an index whose state is not READY, so a chunk committed
    /// under BUILDING publishes nothing: a snapshot opened before the final
    /// flip goes on refusing the index for its whole life, and one opened after
    /// it sees the index whole. Holding the entire build in a single
    /// transaction buys no visibility that this does not, and it costs the
    /// page-WAL's managed-byte allowance -- a build large enough to matter is
    /// refused with `ResourceLimit` rather than being slow.
    ///
    /// The final chunk's entries and the READY flip share one transaction, so
    /// the index becomes visible in one step.
    ///
    /// Sacrifice: build-level crash atomicity. A crash part-way leaves the
    /// BUILDING descriptor and its cursor committed, so partial derived entries
    /// survive the crash. Nothing reads them -- BUILDING is refused by queries
    /// and maintained by ordinary writes -- and the build resumes from the
    /// cursor or is cancelled with `begin_drop_index`. This is exactly what the
    /// resumable policy already accepts; the difference was never visible to a
    /// reader, only to the WAL.
    pub fn build_index_to_ready(&mut self, id: IndexId, chunk_rows: usize) -> Result<usize> {
        self.build_index_to_ready_capped(id, chunk_rows, None)
    }
    /// Same as `build_index_to_ready`, but stop after `max_commits` committed
    /// insert groups and leave the descriptor BUILDING. Sort restarts from
    /// scratch on the next call; already-written keys are overwritten or
    /// skipped. Test hook for crash/resume, not part of the public contract.
    #[doc(hidden)]
    pub fn build_index_to_ready_capped(
        &mut self,
        id: IndexId,
        chunk_rows: usize,
        max_commits: Option<usize>,
    ) -> Result<usize> {
        // A commit is a FULL barrier, and a barrier per 256 rows is the late
        // build's real constant. Measured at 200,000 rows on the reference Mac,
        // for byte-identical indexes: the two scalar builds took 20.8s
        // committing after every chunk and 3.0s committing after every
        // sixteenth, 6.9x for the same work. So group chunks into one
        // transaction.
        //
        // Grouping cannot be allowed to reintroduce the refusal it exists to
        // avoid, and nothing can predict a chunk's byte cost from its row
        // count. So the group is not a guess that has to be right: an allowance
        // refusal rolls back to the last committed cursor -- exactly what
        // BUILDING is for -- halves the group and carries on. At a group of one
        // this is the per-chunk shape, which is the smallest transaction the
        // format admits; a refusal there is a genuinely too-small allowance and
        // is returned to the caller.
        const GROUP: usize = 16;
        // A sorted build does not count chunks: it fills the byte budget
        // `sorted_run_budget` computes from the allowance. This is the group
        // it starts at -- the one that admits the first-run pack, and the
        // scale factor on that budget. Allowance refusal still halves it, and
        // halves the run with it.
        const SORTED_GROUP: usize = SORTED_GROUP_FIRST;
        fn allowance(e: &Error) -> bool {
            matches!(e, Error::Kernel(kernel::Error::ResourceLimit(_)))
        }
        // A BUILD MAY NOT START ON UNCOMMITTED WORK OF THE CALLER'S.
        //
        // Finding the transaction size the allowance admits is part of how
        // this driver works: a refusal rolls back and tries a smaller one. But
        // `rollback` discards the WHOLE open transaction, not the build's
        // share of it, so anything else pending goes with it. When what was
        // pending was the index's own CREATE, the registry row went too and
        // the retry asked about an index that no longer existed --
        // `NotFound("index")` out of a call that was only ever refused for
        // want of allowance, and an index lost by a build that was supposed to
        // be resumable. That is the failure this closes.
        //
        // The question is not "is the handle dirty" but WHOSE work is sitting
        // in the transaction, because a build leaves its own READY flip there
        // for the caller's commit:
        //
        //     create A; build A; create B; build B; commit
        //
        // is a reasonable thing to write, and by the second build the handle
        // is dirty with A's publication and B's creation -- the engine's own
        // work, both of them. Committing that is safe: an index that is finished
        // or empty is not a decision anyone can object to, and it is exactly
        // what stops a later refusal from rolling the create away. So it is
        // committed here.
        //
        // USER writes -- rows, edges, collections, graph names -- are a
        // different matter. Committing them decides something that is not the
        // engine's to decide, and discarding them is worse. Those are refused,
        // with the sentence that fixes it. `Database::user_writes_pending`
        // tells the two apart; every public write entry point sets it.
        //
        // The flag is set on ENTRY to a write, before that write can be
        // refused for a bad field, and it stays set until the next commit or
        // rollback. So a caller whose `put` was rejected is asked to commit
        // before building, and committing an empty transaction costs one
        // barrier and clears it. Conservative in the direction that cannot
        // lose anything, and the remedy is the one the message names.
        if self.user_writes_pending {
            return Err(invalid("commit pending writes before building an index"));
        }
        if self.store.is_dirty() || self.sequence.is_some() {
            self.commit()?;
        }
        let family = self.index_info(id)?.family;
        let sorted = match family {
            IndexFamily::Scalar | IndexFamily::SpatialPoint | IndexFamily::SpatialGeometry => true,
            // A text index can only be packed from a clean slate; otherwise
            // the chunked head-row builder finishes it.
            IndexFamily::Text => {
                crate::index::text::sorted_build_possible(self, &self.index_info(id)?)?
            }
            _ => false,
        };
        if !sorted {
            // A CHUNK COUNT IS NOT A TRANSACTION SIZE -- here too.
            //
            // The sorted path already says this about its own runs (see the
            // comment above the ascending loop in `build_sorted_once`) and
            // ends them on the byte budget instead. This path -- exact and
            // quantized vector locators, and a text index that cannot be
            // packed -- was still committing every `GROUP` chunks whatever
            // those chunks cost. Counted at 50,000 rows on the `battle50k`
            // corpus: `place_emb_exact` took 15 transactions to write 345 WAL
            // page frames (1.49 MB) and `place_emb_ann` 15 to write 825
            // (3.48 MB). One transaction's WAL allowance is 16 MiB. Thirty
            // FULL barriers, measured at 11.9 ms each on the reference
            // volume, for what four would have published.
            //
            // The group stays a CHUNK COUNT, because it is also the bound
            // that keeps a transaction finite and the unit the allowance
            // back-off halves. What changes is that it is no longer a
            // constant: after each committed group the driver knows exactly
            // what that group cost in WAL bytes, and rescales the next one
            // towards `budget`. A first group of `GROUP` chunks is the
            // measurement; every group after it is sized by it.
            //
            // Sacrifice (Law 4): a crash mid-build, or an allowance refusal,
            // discards a larger in-flight group, so the resume from the
            // committed cursor re-does more chunks. Bounded rework, never a
            // wrong answer -- the same sacrifice the sorted path already
            // names, and the refusal back-off (halve and retry, down to one
            // chunk) is unchanged.
            const MAX_GROUP: usize = 1024;
            let budget = (self.store()?.store().wal_allowance() / 4).max(1);
            let mut group = GROUP;
            // A refusal is the allowance telling the driver the size it just
            // tried is too large. Halving and then letting the very next
            // measurement grow the group straight back would make the
            // back-off a loop; the ceiling records what the refusal taught.
            let mut ceiling = MAX_GROUP;
            let mut chunks = 0;
            loop {
                let mut pending = 0;
                let mut at = self.io_counters()?.wal_bytes_written;
                let outcome = loop {
                    let ready = match self.build_index_step(id, chunk_rows) {
                        Ok(ready) => ready,
                        Err(e) if allowance(&e) && group > 1 => break Err(e),
                        Err(e) => return Err(e),
                    };
                    chunks += 1;
                    pending += 1;
                    if ready || pending >= group {
                        let sized = pending;
                        match self.commit() {
                            Ok(()) => pending = 0,
                            Err(e) if allowance(&e) && group > 1 => break Err(e),
                            Err(e) => return Err(e),
                        }
                        // What that group actually cost, and what the next
                        // one may therefore be. Growth is capped at 8x a
                        // step so one cheap group cannot leap straight to a
                        // transaction the allowance refuses; `MAX_GROUP`
                        // caps it outright.
                        let now = self.io_counters()?.wal_bytes_written;
                        let used = now.saturating_sub(at).max(1);
                        at = now;
                        let want = (sized as u64).saturating_mul(budget) / used;
                        group = (want as usize).clamp(1, group.saturating_mul(8).min(ceiling));
                    }
                    if ready {
                        break Ok(chunks);
                    }
                };
                match outcome {
                    Ok(chunks) => return Ok(chunks),
                    Err(_) => {
                        self.rollback()?;
                        chunks -= pending;
                        group /= 2;
                        ceiling = group;
                    }
                }
            }
        }
        let mut group = SORTED_GROUP;
        loop {
            match self.build_sorted_once(id, chunk_rows, group, max_commits) {
                Ok(chunks) => return Ok(chunks),
                Err(e) if allowance(&e) && group > 1 => {
                    self.rollback()?;
                    group /= 2;
                }
                Err(e) => return Err(e),
            }
        }
    }
    /// Sort every derived key, then insert in ascending order. The sort is not
    /// resumable (spill files die with the process); the insert is, by
    /// skipping keys already committed. Cursor after a committed insert group
    /// is the last entity sequence scanned — the whole collection — so a
    /// crash restarts the sort and continues the insert.
    fn build_sorted_once(
        &mut self,
        id: IndexId,
        chunk_rows: usize,
        group: usize,
        max_commits: Option<usize>,
    ) -> Result<usize> {
        self.ready_write()?;
        if !(1..=MAX_BATCH).contains(&chunk_rows) {
            return Err(invalid("index batch must be 1..256"));
        }
        let mut i = self.index_info(id)?;
        match i.state {
            IndexState::Ready => return Ok(0),
            IndexState::Dropping => return Err(invalid("index is dropping")),
            IndexState::Building { .. } => {}
        }
        if i.family == IndexFamily::Text {
            return crate::index::text::build_sorted(self, &mut i, chunk_rows, group, max_commits);
        }
        let mut sorter = super::sort::ExternalSorter::new(
            &self.sort_scratch(),
            super::sort::DEFAULT_BUDGET,
        )?;
        // One scratch pair for the whole scan. See `skey_into`.
        let mut value_buf = Vec::new();
        let mut key_buf = Vec::new();
        let max_seq = self.scan_collection_rows(i.collection, |eid, row| {
            match i.family {
                IndexFamily::Scalar => {
                    self.scalar_build_key_into(&i, row, &mut value_buf)?;
                    skey_into(&i, &value_buf, eid.sequence, &mut key_buf);
                    // `push_ref`, not `push`: the sorter copies the key into
                    // its own storage either way, and this way the build does
                    // not also allocate the key it is copying FROM.
                    sorter.push_ref(&key_buf, &[])?;
                }
                IndexFamily::SpatialPoint => {
                    if let Some(point) =
                        crate::index::spatial::point::build_point_entry(self, &i, eid, row)?
                    {
                        sorter.push(point.key, point.value.to_vec())?;
                    }
                }
                IndexFamily::SpatialGeometry => {
                    for entry in
                        crate::index::spatial::geometry_index::build_geometry_entries(self, &i, eid, row)?
                    {
                        sorter.push(entry.key, entry.value.to_vec())?;
                    }
                }
                _ => unreachable!(),
            }
            Ok(())
        })?;
        let mut merge = sorter.finish()?;
        // A version-2 index owns an EMPTY tree, and the sorted stream is
        // exactly the tree's final contents in order. That is the one shape a
        // bottom-up pack is for: leaves filled to 90% and written once, the
        // interior levels built from the separators those leaves produced, and
        // no descent at all -- the cost is the pages the index occupies, not
        // the keys it holds.
        //
        // Every page is an ordinary pooled page, so this is one logged
        // transaction like any other write: no root swap, no skipped WAL, and
        // a crash before the commit leaves nothing (the descriptor still says
        // BUILDING with root 0, and the next call re-sorts and re-packs).
        //
        // PACK vs ASCENDING PUT. The pack is tried once, on the first attempt
        // at the largest group. If it is refused for want of WAL allowance --
        // the whole index is one transaction, and a bounded database may not
        // have room for it -- the outer loop halves the group and comes back
        // here, and every later attempt takes the ascending-put path below.
        // That path is resumable in committed groups, and into a FRESH,
        // EMPTY, single-family tree every insert is an append at the right
        // edge, which is the kernel's cheapest split. It is slower than the
        // pack and it is never wrong.
        //
        // BOUNDED RUNS. The pack is no longer offered the whole index. It is
        // offered the FIRST RUN: as many entries as `sorted_run_budget` says
        // one transaction can afford. If the sorted stream ends inside that
        // budget -- which is every index this engine was measured on below
        // roughly half a million rows -- nothing else happens and the build is
        // the single pack it always was. If it does not, the packed run is
        // committed with its root, and the remainder appends at the right edge
        // of that same tree through the ascending path below, committing on
        // the same budget. So the build completes at ANY size under a fixed
        // allowance, and a database large enough to refuse the whole-index
        // pack no longer discovers that by being refused.
        //
        // WHY NOT GRAFT THE LATER RUNS. `BTree::graft_sorted_range` packs a
        // run and splices it in as one subtree, which is what the later runs
        // want. It enters the standing tree as a single separator, so the
        // grafted subtree's height is independent of the tree's and the tree
        // stops being uniformly deep -- and at the right edge, EVERY run would
        // graft there, adding a level each time until the format's depth bound
        // refused the build. A right-edge graft is an append only in key
        // order, not in shape. The ascending put costs a descent per key and
        // keeps one uniformly deep tree, and because a per-keyspace append
        // splits at the right edge rather than down the middle, its leaves
        // come out essentially full -- denser than the 0.9 the pack targets.
        // Measured leaf occupancy is asserted in tests/index_build_equivalence.rs.
        let mut carry: Option<(Vec<u8>, Vec<u8>)> = None;
        let mut packed_tail: Option<Vec<u8>> = None;
        if let Some(t) = i.tree {
            if t.root == 0 && group == SORTED_GROUP_FIRST && max_commits.is_none() {
                let budget = self.sorted_run_budget()?;
                let mut used = 0u64;
                let mut cut: Option<(Vec<u8>, Vec<u8>)> = None;
                let unique = i.family == IndexFamily::Scalar && i.unique;
                let kind = i.kind.clone();
                let head = ikey(SCALAR, i.id).len();
                let scratch = self.sort_scratch();
                let mut duplicate = false;
                let mut prev: Option<Vec<u8>> = None;
                let mut failed = None;
                let stream = std::iter::from_fn(|| match merge.next_entry() {
                    Ok(Some((key, value))) => {
                        // What this entry costs a packed leaf: its record plus
                        // its slot. `enc_leaf` frames a record in at most four
                        // bytes and a slot is four more, so key + value + 8 is
                        // never an under-estimate, whichever cell encoding the
                        // build was compiled for.
                        let need = (key.len() + value.len() + 8) as u64;
                        if used > 0 && used + need > budget {
                            cut = Some((key, value));
                            return None;
                        }
                        used += need;
                        if unique {
                            match scalar_key::decode(&kind, &key[head..]) {
                                Ok((_, n)) => {
                                    let v = &key[head..head + n];
                                    if v == [0] {
                                        prev = None;
                                    } else if prev.as_deref() == Some(v) {
                                        duplicate = true;
                                        return None;
                                    } else {
                                        prev = Some(v.to_vec());
                                    }
                                }
                                Err(e) => {
                                    failed = Some(e);
                                    return None;
                                }
                            }
                        }
                        Some(Ok((key, value, false)))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        failed = Some(e);
                        None
                    }
                });
                let packed = self.writer()?.tree_pack(t.id, stream, 0.9, &scratch);
                if let Some(e) = failed {
                    return Err(e);
                }
                if duplicate {
                    return Err(Error::AlreadyExists);
                }
                let (root, _rows) = packed?;
                i.tree = Some(IndexTree { id: t.id, root });
                if cut.is_none() {
                    i.state = IndexState::Ready;
                    self.save_index(&i)?;
                    self.commit()?;
                    return Ok((max_seq as usize).div_ceil(chunk_rows).max(1));
                }
                // More to come. The root is durable in the SAME transaction as
                // the pages it names, and the descriptor still says BUILDING,
                // so a crash here leaves a partial index nothing reads and the
                // next call re-sorts and resumes over it.
                self.save_index(&i)?;
                self.commit()?;
                // The unique check compares each value against the one before
                // it, and the run boundary is a pair like any other: hand the
                // last packed value to the loop below so the seam is checked.
                packed_tail = prev;
                carry = cut;
            }
        }
        // WHAT ENDS AN ASCENDING RUN: the byte budget, and only the byte budget.
        //
        // `SORTED_GROUP_FIRST` names the group the first PACK attempt is
        // offered, above. It was ALSO being used down here as a chunk-count
        // commit throttle, and a chunk count is not a transaction size: 64
        // chunks of 256 rows ends a run every 16,384 rows, about a seventh of
        // the ~115,000 entries `sorted_run_budget` says one transaction can
        // afford, so the budget never got to fire first. Every commit is a
        // full barrier. Measured at 1,000,000 rows: 54 ascending commits at
        // ~25 ms each (~47 ms on the one in six that also folds the WAL),
        // which was the majority of the whole build's wall clock, for runs the
        // allowance would have taken seven times larger.
        //
        // The refusal back-off still has something to halve. `group` now
        // SCALES the budget instead of counting chunks, so a `ResourceLimit`
        // comes back here with half the run and the same single trigger, down
        // to a sixty-fourth of it before the refusal is returned to the caller.
        //
        // Sacrifice (Law 4): a crash mid-build discards a larger in-flight
        // group, so the resume from the committed cursor re-inserts more keys.
        // Bounded rework, never a wrong answer -- and the sort itself already
        // restarts from scratch on every call whatever the group size was.
        let run_budget = match max_commits {
            // The crash/resume hook exists to produce committed groups to stop
            // after, so it asks for the smallest run this loop can make: one
            // chunk. Still the byte trigger, with a budget of one byte.
            Some(_) => 1,
            None => (self.sorted_run_budget()? * group as u64 / SORTED_GROUP_FIRST as u64).max(1),
        };
        let mut run_bytes = 0u64;
        let mut pending = 0usize;
        let mut commits = 0usize;
        let mut prev_value: Option<Vec<u8>> = packed_tail;
        while let Some((key, value)) = match carry.take() {
            Some(entry) => Some(entry),
            None => merge.next_entry()?,
        } {
            if i.family == IndexFamily::Scalar && i.unique {
                let v = self.scalar_value_bytes(&i, &key)?;
                if v != [0] {
                    if prev_value.as_deref() == Some(v) {
                        return Err(Error::AlreadyExists);
                    }
                    prev_value = Some(v.to_vec());
                } else {
                    prev_value = None;
                }
            }
            run_bytes += (key.len() + value.len() + 8) as u64;
            self.index_put(&mut i, &key, &value)?;
            pending += 1;
            if pending >= chunk_rows {
                pending = 0;
                // One reason to commit: the run is as large as the allowance
                // affords. Rows are the wrong unit for it -- a group of small
                // scalar keys is well inside the allowance where the same
                // group of spatial entries with their payloads need not be --
                // and the chunk boundary is only where the question is asked.
                if run_bytes >= run_budget {
                    run_bytes = 0;
                    self.commit()?;
                    commits += 1;
                    if max_commits.is_some_and(|n| commits >= n) {
                        i.state = IndexState::Building { after: max_seq };
                        self.save_index(&i)?;
                        self.commit()?;
                        return Ok((max_seq as usize).div_ceil(chunk_rows).max(1));
                    }
                }
            }
        }
        i.state = IndexState::Ready;
        self.save_index(&i)?;
        self.commit()?;
        Ok((max_seq as usize).div_ceil(chunk_rows).max(1))
    }
    pub fn begin_drop_index(&mut self, id: IndexId) -> Result<()> {
        self.ready_write()?;
        let mut i = self.index_info(id)?;
        i.state = IndexState::Dropping;
        let result = self.save_index(&i);
        self.finish(result)
    }
    /// Cancel a BUILDING index with begin_drop_index, then reclaim bounded steps.
    /// The versioned header stays enabled even after its last index is removed.
    pub fn drop_index_step(&mut self, id: IndexId, batch: usize) -> Result<bool> {
        self.ready_write()?;
        if !(1..=MAX_BATCH).contains(&batch) {
            return Err(invalid("index batch must be 1..256"));
        }
        let mut i = self.index_info(id)?;
        if i.state != IndexState::Dropping {
            return Err(invalid("call begin_drop_index first"));
        }
        let (keys, end) = if i.family == IndexFamily::Text {
            crate::index::text::drop_batch(self, id, batch)?
        } else {
            let p = match i.family {
                IndexFamily::Scalar => ikey(SCALAR, id),
                IndexFamily::ExactVector => crate::index::vector::exact::locator_prefix(id),
                IndexFamily::QuantizedVector => crate::index::vector::quantized::entry_prefix(id),
                IndexFamily::SpatialPoint => crate::index::spatial::point::posting_prefix(id),
                IndexFamily::SpatialGeometry => crate::index::spatial::geometry_index::posting_prefix(id),
                IndexFamily::Text => unreachable!(),
            };
            let mut keys = Vec::new();
            let mut end = true;
            for row in self.index_range(&i, &p)?.into_iter().flatten() {
                let (k, _) = row?;
                if !k.starts_with(&p) {
                    break;
                }
                if keys.len() == batch {
                    end = false;
                    break;
                }
                keys.push(k);
            }
            (keys, end)
        };
        let result = (|| {
            for k in keys {
                self.index_delete(&mut i, &k)?;
            }
            if end {
                // The tree's leaves and interiors were freed as the deletes
                // emptied them; the root of an emptied tree is the one page
                // nothing above it can unlink, so the drop returns it here,
                // in the same bounded step that removes the descriptor.
                if let Some(t) = i.tree {
                    self.writer()?.tree_free_root(t.id, t.root)?;
                }
                self.index_descriptors_changed();
                for copy in 0..3 {
                    self.writer()?.delete(&dkey(id, copy))?;
                }
                self.writer()?.delete(&ikey(REGISTRY, id))?;
                self.writer()?.delete(&ckey(i.collection, id))?;
                self.writer()?.delete(&nkey(i.collection, &i.name))?;
                let mut h = self
                    .index_header
                    .ok_or_else(|| corrupt("missing index header"))?;
                h.count = h
                    .count
                    .checked_sub(1)
                    .ok_or_else(|| corrupt("index count underflow"))?;
                let (nc, nl) = self.header()?;
                self.index_header = Some(h);
                self.write_header(nc, nl)?;
            }
            Ok(end)
        })();
        self.finish(result)
    }
    pub fn query_scalar(
        &self,
        id: IndexId,
        predicate: ScalarPredicate,
        limit: usize,
    ) -> Result<Vec<EntityId>> {
        if limit > MAX_RESULTS {
            return Err(invalid("query result limit exceeds 65536"));
        }
        let i = self.index_info(id)?;
        if i.family != IndexFamily::Scalar {
            return Err(invalid("index is not a scalar index"));
        }
        if i.state != IndexState::Ready {
            return Err(invalid("index is not ready"));
        }
        if limit == 0 {
            return Ok(vec![]);
        }
        let (lower, upper) = match predicate {
            ScalarPredicate::Eq(v) => {
                let k = scalar_key::encode(&i.kind, Some(&v))?;
                (Some(k.clone()), Some(k))
            }
            ScalarPredicate::Range { lower, upper } => (
                lower
                    .as_ref()
                    .map(|v| scalar_key::encode(&i.kind, Some(v)))
                    .transpose()?,
                upper
                    .as_ref()
                    .map(|v| scalar_key::encode(&i.kind, Some(v)))
                    .transpose()?,
            ),
        };
        if matches!((&lower,&upper),(Some(a),Some(b)) if a>b) {
            return Ok(vec![]);
        }
        let p = ikey(SCALAR, id);
        let mut start = p.clone();
        if let Some(k) = lower {
            start.extend(k);
        }
        let mut out = Vec::new();
        for row in self.index_range(&i, &start)?.into_iter().flatten() {
            let (key, value) = row?;
            if !key.starts_with(&p) {
                break;
            }
            let (_, n) = scalar_key::decode(&i.kind, &key[p.len()..])?;
            if upper
                .as_ref()
                .is_some_and(|u| &key[p.len()..p.len() + n] > u.as_slice())
            {
                break;
            }
            let mut at = p.len() + n;
            let seq = read_ordered(&key, &mut at)?;
            if at != key.len() || seq == 0 || !value.is_empty() {
                return Err(corrupt("scalar index entry"));
            }
            out.push(EntityId {
                collection: i.collection,
                sequence: seq,
            });
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn scalar_descriptor_bytes_remain_frozen() {
        let info = IndexInfo {
            id: IndexId(9),
            collection: CollectionId(7),
            name: "by_age".into(),
            field: "age".into(),
            family: IndexFamily::Scalar,
            kind: Kind::Int,
            unique: true,
            state: IndexState::Building { after: 42 },
            encoding_version: 1,
            tree: None,
            expression: None,
        };
        let mut payload = 9u64.to_be_bytes().to_vec();
        payload.extend(7u32.to_be_bytes());
        payload.push(1);
        payload.extend(1u16.to_be_bytes());
        payload.push(2);
        payload.push(1);
        payload.push(0);
        payload.extend(42u64.to_be_bytes());
        payload.extend(6u16.to_be_bytes());
        payload.extend(b"by_age");
        payload.extend(3u16.to_be_bytes());
        payload.extend(b"age");
        assert_eq!(encode(&info).unwrap(), packet(MAGIC, &payload).unwrap());
        assert_eq!(decode(&encode(&info).unwrap()).unwrap(), info);
    }

    #[test]
    fn exact_vector_descriptor_dispatch_and_unknown_options() {
        let info = IndexInfo {
            id: IndexId(3),
            collection: CollectionId(2),
            name: "embedding".into(),
            field: "v".into(),
            family: IndexFamily::ExactVector,
            kind: Kind::Vector(1536),
            unique: false,
            state: IndexState::Ready,
            encoding_version: 1,
            tree: None,
            expression: None,
        };
        let encoded = encode(&info).unwrap();
        assert_eq!(decode(&encoded).unwrap(), info);
        let mut future = encoded;
        future[10 + 19] = 1;
        let n = future.len();
        let crc = crc32c::crc32c(&future[..n - 4]).to_le_bytes();
        future[n - 4..].copy_from_slice(&crc);
        assert!(matches!(decode(&future), Err(Error::Unsupported(_))));
    }

    #[test]
    fn text_descriptor_pins_analyzer_unicode_bm25_and_options() {
        let info = IndexInfo {
            id: IndexId(5),
            collection: CollectionId(4),
            name: "body_text".into(),
            field: "body".into(),
            family: IndexFamily::Text,
            kind: Kind::Text,
            unique: false,
            state: IndexState::Building { after: 19 },
            encoding_version: 1,
            tree: None,
            expression: None,
        };
        let encoded = encode(&info).unwrap();
        assert_eq!(decode(&encoded).unwrap(), info);
        let mut future = encoded;
        future[10 + 22] = 1;
        let n = future.len();
        let crc = crc32c::crc32c(&future[..n - 4]).to_le_bytes();
        future[n - 4..].copy_from_slice(&crc);
        assert!(matches!(decode(&future), Err(Error::Unsupported(_))));
    }

    #[test]
    fn quantized_vector_descriptor_pins_quantizer_and_options() {
        let info = IndexInfo {
            id: IndexId(6),
            collection: CollectionId(4),
            name: "embedding_int8".into(),
            field: "embedding".into(),
            family: IndexFamily::QuantizedVector,
            kind: Kind::Vector(1536),
            unique: false,
            state: IndexState::Building { after: 91 },
            encoding_version: 1,
            tree: None,
            expression: None,
        };
        let encoded = encode(&info).unwrap();
        assert_eq!(decode(&encoded).unwrap(), info);
        for payload_offset in [19, 20] {
            let mut future = encoded.clone();
            future[10 + payload_offset] = 2;
            let n = future.len();
            let crc = crc32c::crc32c(&future[..n - 4]).to_le_bytes();
            future[n - 4..].copy_from_slice(&crc);
            assert!(matches!(decode(&future), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn spatial_geometry_descriptor_pins_ladder_and_cell_budget() {
        let info = IndexInfo {
            id: IndexId(8),
            collection: CollectionId(4),
            name: "by_shape".into(),
            field: "shape".into(),
            family: IndexFamily::SpatialGeometry,
            kind: Kind::Geo,
            unique: false,
            state: IndexState::Building { after: 7 },
            encoding_version: 1,
            tree: None,
            expression: None,
        };
        let encoded = encode(&info).unwrap();
        assert_eq!(decode(&encoded).unwrap(), info);
        // Any of the ladder/cell-budget/reserved bytes moving is a future,
        // incompatible layout: refused whole, never silently reinterpreted.
        for payload_offset in [15, 16, 17, 18, 19] {
            let mut future = encoded.clone();
            future[10 + payload_offset] = 0xff;
            let n = future.len();
            let crc = crc32c::crc32c(&future[..n - 4]).to_le_bytes();
            future[n - 4..].copy_from_slice(&crc);
            assert!(matches!(decode(&future), Err(Error::Unsupported(_))));
        }
    }
}

#[cfg(test)]
#[path = "../faults/index_fault_tests.rs"]
mod fault_tests;
