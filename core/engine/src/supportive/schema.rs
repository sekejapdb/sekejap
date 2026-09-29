//! Build steps 3b and 3c: the entries of 2.b and 2.c that describe tables --
//! `NAME`, `nIDX`, `TABL`, `COLM`, `LAYT`, `KEYS` -- and the `BIND`, `MEMB`
//! and `JOBS` entries a table owns (`docs/core/SUPPORTIVE.md` 2.i).
//!
//! This module is bytes only: each entry's payload as a struct, its key, and
//! the census line it needs. The catalog code in `collections` turns a
//! table's entries into the engine's in-memory catalog record and back.

use super::carrier::{CensusLine, Item, Key, Kind, Node, OwnerClass};
use crate::collections::{corrupt, invalid, Error};

type Result<T> = std::result::Result<T, Error>;

/// Column ids 1-15 are reserved for the columns the engine manages; the
/// first user column is 16.
pub(crate) const COLUMN_KEY: u64 = 1;
pub(crate) const COLUMN_CREATED: u64 = 2;
pub(crate) const COLUMN_UPDATED: u64 = 3;
pub(crate) const FIRST_USER_COLUMN: u64 = 16;

/// `NAME` classes (2.i).
pub(crate) const NAME_SCHEMA: u8 = 1;
pub(crate) const NAME_TABLE: u8 = 2;
pub(crate) const NAME_INDEX: u8 = 4;
pub(crate) const NAME_EDGE_TYPE: u8 = 5;
pub(crate) const NAME_CONTEXT: u8 = 6;

/// `NEXT` classes of the graph name allocators.
pub(crate) const NEXT_EDGE_TYPE: u64 = 6;
pub(crate) const NEXT_CONTEXT: u64 = 7;

/// `NEXT` id classes owned below the database (2.i).
pub(crate) const NEXT_SCHEMA: u64 = 9;
pub(crate) const NEXT_COLUMN: u64 = 11;

/// At most this many slots in one `LAYT` part.
pub(crate) const SLOTS_PER_PART: usize = 512;

pub(crate) fn kind(code: &[u8; 4]) -> Kind {
    Kind::new(code).expect("a registry kind is well formed")
}
pub(crate) fn line(code: &[u8; 4], version: u8, variant: u32) -> CensusLine {
    CensusLine { kind: kind(code), version, variant }
}

fn key(node: Node, owner_class: OwnerClass, owner_id: u64, code: &[u8; 4], item: Item) -> Key {
    Key { node, owner_class, owner_id, kind: kind(code), item }
}
pub(crate) fn table_key(table: u64) -> Key {
    key(Node::C, OwnerClass::Table, table, b"TABL", Item::Id(0))
}
pub(crate) fn column_key(table: u64, column: u64) -> Key {
    key(Node::C, OwnerClass::Table, table, b"COLM", Item::Id(column))
}
pub(crate) fn columns_prefix(table: u64) -> Vec<u8> {
    Key::prefix(Node::C, OwnerClass::Table, table, kind(b"COLM"))
}
pub(crate) fn keys_key(table: u64) -> Key {
    key(Node::C, OwnerClass::Table, table, b"KEYS", Item::Id(0))
}
pub(crate) fn bind_key(table: u64) -> Key {
    key(Node::E, OwnerClass::Table, table, b"BIND", Item::Id(0))
}
pub(crate) fn memb_key(table: u64) -> Key {
    key(Node::E, OwnerClass::Table, table, b"MEMB", Item::Id(0))
}
/// The table-drop job of a table: one per table, keyed by the job type.
pub(crate) fn drop_job_key(table: u64) -> Key {
    key(Node::F, OwnerClass::Table, table, b"JOBS", Item::Id(3))
}
/// The column allocator of a table.
pub(crate) fn column_next_key(table: u64) -> Key {
    key(Node::B, OwnerClass::Table, table, b"NEXT", Item::Id(NEXT_COLUMN))
}
pub(crate) fn schema_next_key() -> Key {
    key(Node::B, OwnerClass::Database, 0, b"NEXT", Item::Id(NEXT_SCHEMA))
}
/// The `NAME` of a table or a schema.
pub(crate) fn name_key(class: u8, id: u64) -> Key {
    let owner = if class == NAME_SCHEMA { OwnerClass::Schema } else { OwnerClass::Table };
    key(Node::B, owner, id, b"NAME", Item::Id(0))
}
/// `nIDX`: a table name under its schema (0 is `public`), or a schema name
/// under the database.
pub(crate) fn name_index_key(class: u8, schema: u64, name: &str) -> Key {
    let (owner, id) = if class == NAME_SCHEMA { (OwnerClass::Database, 0) } else { (OwnerClass::Schema, schema) };
    key(Node::B, owner, id, b"nIDX", Item::Name { class, name: name.as_bytes().to_vec() })
}
/// Every `nIDX` of one class under one owner.
pub(crate) fn name_index_prefix(class: u8, schema: u64) -> Vec<u8> {
    let (owner, id) = if class == NAME_SCHEMA { (OwnerClass::Database, 0) } else { (OwnerClass::Schema, schema) };
    let mut p = Key::prefix(Node::B, owner, id, kind(b"nIDX"));
    p.push(class);
    p
}
/// `INDX` of an index, owned by the index itself so it is found from its id.
pub(crate) fn index_key(index: u64) -> Key {
    key(Node::D, OwnerClass::Index, index, b"INDX", Item::Id(0))
}
/// Every `INDX` of the database.
pub(crate) fn indexes_prefix() -> Vec<u8> {
    vec![b'd', OwnerClass::Index as u8]
}
/// `TREE` of an index's own B-tree.
pub(crate) fn tree_key(index: u64, tree: u16) -> Key {
    key(Node::A, OwnerClass::Index, index, b"TREE", Item::Id(u64::from(tree)))
}
/// `nIDX` of an index name under its table.
pub(crate) fn index_name_key(table: u64, name: &str) -> Key {
    key(Node::B, OwnerClass::Table, table, b"nIDX", Item::Name { class: NAME_INDEX, name: name.as_bytes().to_vec() })
}
pub(crate) fn index_names_prefix(table: u64) -> Vec<u8> {
    let mut p = Key::prefix(Node::B, OwnerClass::Table, table, kind(b"nIDX"));
    p.push(NAME_INDEX);
    p
}
/// `INDX` census line: version 1 plain, 2 `lower`, 3 JSON member; variant
/// the family code of 2.i.
pub(crate) fn index_line(family: u8, expression: u8) -> CensusLine {
    line(b"INDX", 1 + expression, u32::from(family))
}
/// `GRPH` of the base graph (graph id 0).
pub(crate) fn graph_key() -> Key {
    key(Node::E, OwnerClass::Graph, 0, b"GRPH", Item::Id(0))
}
pub(crate) fn graph_next_key(class: u64) -> Key {
    key(Node::B, OwnerClass::Database, 0, b"NEXT", Item::Id(class))
}
/// `NAME` of an edge type (class 5) or a graph context (class 6).
pub(crate) fn graph_name_key(class: u8, id: u64) -> Key {
    let owner = if class == NAME_EDGE_TYPE { OwnerClass::EdgeType } else { OwnerClass::Graph };
    key(Node::B, owner, id, b"NAME", Item::Id(0))
}
/// `nIDX` of an edge type or context name, under the database.
pub(crate) fn graph_name_index_key(class: u8, name: &str) -> Key {
    key(Node::B, OwnerClass::Database, 0, b"nIDX", Item::Name { class, name: name.as_bytes().to_vec() })
}
pub(crate) fn graph_name_index_prefix(class: u8) -> Vec<u8> {
    let mut p = Key::prefix(Node::B, OwnerClass::Database, 0, kind(b"nIDX"));
    p.push(class);
    p
}
/// `NEXT` class 4: a table's row sequence.
pub(crate) const NEXT_ROW: u64 = 4;
/// `NEXT` class 5: the edge-id allocator.
pub(crate) const NEXT_EDGE_ID: u64 = 5;
pub(crate) fn sequence_key(table: u64) -> Key {
    key(Node::B, OwnerClass::Table, table, b"NEXT", Item::Id(NEXT_ROW))
}
pub(crate) fn edge_id_key() -> Key {
    key(Node::B, OwnerClass::Database, 0, b"NEXT", Item::Id(NEXT_EDGE_ID))
}
/// 2.g statistics, ignorable: a table's live row count, a text index's
/// corpus totals, a Vamana index's entry point.
pub(crate) fn row_count_key(table: u64) -> Key {
    key(Node::G, OwnerClass::Table, table, b"rCNT", Item::Id(0))
}
pub(crate) fn corpus_key(index: u64) -> Key {
    key(Node::G, OwnerClass::Index, index, b"tCRP", Item::Id(0))
}
pub(crate) fn vamana_key(index: u64) -> Key {
    key(Node::G, OwnerClass::Index, index, b"vENT", Item::Id(0))
}
pub(crate) fn layout_key(layout: u64, part: u8) -> Key {
    key(Node::C, OwnerClass::Database, 0, b"LAYT", Item::Part { id: layout, part })
}

/// Reads a payload field by field; any shortfall is corruption.
pub(crate) struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let out = self.b.get(self.at..self.at + n).ok_or_else(|| corrupt("Register payload too short"))?;
        self.at += n;
        Ok(out)
    }
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// A string with a u8 length.
    pub(crate) fn text(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| corrupt("Register payload text"))
    }
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let out = &self.b[self.at..];
        self.at = self.b.len();
        out
    }
    pub(crate) fn end(&self) -> Result<()> {
        if self.at == self.b.len() {
            Ok(())
        } else {
            Err(corrupt("Register payload has trailing bytes"))
        }
    }
}
pub(crate) fn put_text(b: &mut Vec<u8>, s: &str) -> Result<()> {
    let n = u8::try_from(s.len()).map_err(|_| invalid("a name is at most 255 bytes"))?;
    b.push(n);
    b.extend_from_slice(s.as_bytes());
    Ok(())
}

/// `NAME` version 1: `parent u64 | state u8 (0 live) | name`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Name {
    pub(crate) parent: u64,
    pub(crate) name: String,
}
impl Name {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut b = self.parent.to_be_bytes().to_vec();
        b.push(0);
        put_text(&mut b, &self.name)?;
        Ok(b)
    }
    pub(crate) fn decode(p: &[u8]) -> Result<Self> {
        let mut r = Reader::new(p);
        let parent = r.u64()?;
        if r.u8()? != 0 {
            return Err(corrupt("NAME state"));
        }
        let name = r.text()?;
        r.end()?;
        if name.is_empty() {
            return Err(corrupt("empty NAME"));
        }
        Ok(Self { parent, name })
    }
}

/// `TABL` version 1: `flags u8 (bit 0 timestamps) | current layout u32 |
/// schema u64 (0 is public)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Table {
    pub(crate) timestamps: bool,
    pub(crate) layout: u32,
    pub(crate) schema: u64,
}
impl Table {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = vec![u8::from(self.timestamps)];
        b.extend_from_slice(&self.layout.to_be_bytes());
        b.extend_from_slice(&self.schema.to_be_bytes());
        b
    }
    pub(crate) fn decode(p: &[u8]) -> Result<Self> {
        let mut r = Reader::new(p);
        let flags = r.u8()?;
        if flags & !1 != 0 {
            return Err(corrupt("TABL flags"));
        }
        let layout = r.u32()?;
        let schema = r.u64()?;
        r.end()?;
        if layout == 0 {
            return Err(corrupt("TABL layout"));
        }
        Ok(Self { timestamps: flags & 1 == 1, layout, schema })
    }
}

/// A column's physical type as a layout slot stores it: the 0.18 layout kind
/// code (0-7) and, for a vector, its dimension.
pub(crate) fn kind_code(k: &crate::Kind) -> (u8, u32) {
    use crate::Kind::*;
    match k {
        Text => (0, 0),
        Int => (1, 0),
        Real => (2, 0),
        Bool => (3, 0),
        Json => (4, 0),
        Geo => (5, 0),
        Vector(d) => (6, *d as u32),
        Point => (7, 0),
    }
}
pub(crate) fn kind_from(code: u8, dim: u32) -> Result<crate::Kind> {
    use crate::Kind::*;
    Ok(match code {
        0 => Text,
        1 => Int,
        2 => Real,
        3 => Bool,
        4 => Json,
        5 => Geo,
        6 => Vector(dim as usize),
        7 => Point,
        _ => return Err(Error::Unsupported(format!("needs a newer sekejap: column type {code}"))),
    })
}

/// `COLM` version 1: `state u8 (0 live, 1 retired) | name | stored token |
/// declared spelling (empty: none) | rule length u16 | rule | missing length
/// u32 | missing`. The rule is the 0.18 column-rule encoding. `missing` is the
/// JSON text of the value a row written BEFORE the column existed reads for
/// it (F2: `ADD COLUMN ... DEFAULT` on a table with rows); empty when there
/// is none. The type lives in the layout slots, so a column's history of
/// types is its layouts'.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Column {
    pub(crate) live: bool,
    pub(crate) name: String,
    pub(crate) stored_token: String,
    pub(crate) declared: Option<String>,
    pub(crate) rule: Option<Vec<u8>>,
    pub(crate) missing: Option<serde_json::Value>,
}
impl Column {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut b = vec![u8::from(!self.live)];
        put_text(&mut b, &self.name)?;
        put_text(&mut b, &self.stored_token)?;
        put_text(&mut b, self.declared.as_deref().unwrap_or(""))?;
        let rule = self.rule.as_deref().unwrap_or(&[]);
        let n = u16::try_from(rule.len()).map_err(|_| invalid("column rule too long"))?;
        b.extend_from_slice(&n.to_be_bytes());
        b.extend_from_slice(rule);
        let missing = match &self.missing {
            Some(v) => serde_json::to_vec(v).map_err(invalid)?,
            None => Vec::new(),
        };
        b.extend_from_slice(&(missing.len() as u32).to_be_bytes());
        b.extend_from_slice(&missing);
        Ok(b)
    }
    pub(crate) fn decode(p: &[u8]) -> Result<Self> {
        let mut r = Reader::new(p);
        let live = match r.u8()? {
            0 => true,
            1 => false,
            _ => return Err(corrupt("COLM state")),
        };
        let name = r.text()?;
        let stored_token = r.text()?;
        let declared = Some(r.text()?).filter(|d| !d.is_empty());
        let n = u16::from_be_bytes(r.take(2)?.try_into().unwrap()) as usize;
        let rule = Some(r.take(n)?.to_vec()).filter(|x| !x.is_empty());
        let m = r.u32()? as usize;
        let missing = match r.take(m)? {
            [] => None,
            json => Some(serde_json::from_slice(json).map_err(|_| corrupt("COLM missing value"))?),
        };
        r.end()?;
        if name.is_empty() || stored_token.is_empty() {
            return Err(corrupt("COLM name"));
        }
        Ok(Self { live, name, stored_token, declared, rule, missing })
    }
}

/// One `LAYT` part, version 1: `table u32 | slot count u16 | per slot:
/// column u64 | type u8 | vector dimension u32`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayoutPart {
    pub(crate) table: u32,
    pub(crate) slots: Vec<(u64, u8, u32)>,
}
impl LayoutPart {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = self.table.to_be_bytes().to_vec();
        b.extend_from_slice(&(self.slots.len() as u16).to_be_bytes());
        for (column, code, dim) in &self.slots {
            b.extend_from_slice(&column.to_be_bytes());
            b.push(*code);
            b.extend_from_slice(&dim.to_be_bytes());
        }
        b
    }
    pub(crate) fn decode(p: &[u8]) -> Result<Self> {
        let mut r = Reader::new(p);
        let table = r.u32()?;
        let n = u16::from_be_bytes(r.take(2)?.try_into().unwrap()) as usize;
        if n > SLOTS_PER_PART {
            return Err(corrupt("LAYT part slot count"));
        }
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            slots.push((r.u64()?, r.u8()?, r.u32()?));
        }
        r.end()?;
        Ok(Self { table, slots })
    }
    /// The census variant of a layout: its highest physical type.
    pub(crate) fn variant(parts: &[Self]) -> u32 {
        parts.iter().flat_map(|p| p.slots.iter()).map(|s| u32::from(s.1)).max().unwrap_or(0)
    }
}

/// The census lines this module's entries may carry.
pub(crate) fn lines() -> Vec<CensusLine> {
    let mut out = vec![
        line(b"TABL", 1, 0),
        line(b"COLM", 1, 0),
        line(b"NAME", 1, u32::from(NAME_SCHEMA)),
        line(b"NAME", 1, u32::from(NAME_TABLE)),
        line(b"nIDX", 1, 0),
        line(b"NEXT", 1, NEXT_SCHEMA as u32),
        line(b"NEXT", 1, NEXT_COLUMN as u32),
    ];
    out.extend((0..=7).map(|k| line(b"LAYT", 1, k)));
    for family in [0u8, 1, 3, 4, 5, 6, 7, 8] {
        for expression in 0..3u8 {
            out.push(index_line(family, expression));
        }
    }
    out.push(line(b"TREE", 1, 0));
    out.push(line(b"NAME", 1, u32::from(NAME_EDGE_TYPE)));
    out.push(line(b"NAME", 1, u32::from(NAME_CONTEXT)));
    out.push(line(b"NEXT", 1, NEXT_EDGE_TYPE as u32));
    out.push(line(b"NEXT", 1, NEXT_CONTEXT as u32));
    out.push(line(b"NEXT", 1, NEXT_ROW as u32));
    out.push(line(b"tCRP", 1, 0));
    out.push(line(b"vENT", 1, 0));
    out
}
