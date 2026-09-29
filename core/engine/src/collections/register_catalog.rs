//! A table's catalog record on a Register file (`docs/core/SUPPORTIVE.md`
//! 2.b, 2.c, 2.e, 2.f): the in-memory [`Catalog`] and [`Layout`] the engine
//! reads, built from and written to the entries of
//! `crate::supportive::schema`.
//!
//! What moves where:
//!
//! * name and schema -> `NAME` (and the `nIDX` lookup the name functions keep);
//! * timestamps flag, current layout, schema id -> `TABL`;
//! * declared spellings and column rules -> the column's `COLM`;
//! * key declaration -> `KEYS`; edge-table record -> `BIND`; memberships ->
//!   `MEMB`; a running drop -> the table-drop `JOBS` entry;
//! * a layout -> `LAYT` parts of (column id, type, dimension) slots. Column
//!   ids are allocated per table the first time a layout names a column, and
//!   a slot resolves to the column's current name when it is read.

use super::*;
use crate::supportive::header::Supportive;
use crate::supportive::register::{Trees, TreesMut};
use crate::supportive::schema::{self as sch, line, Column, LayoutPart, Name, Table};
use std::collections::BTreeMap;

fn u64_payload(p: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(p.try_into().map_err(|_| corrupt("Register id payload"))?))
}

/// The id of a schema: 0 for `public`, `None` when a named schema does not
/// exist.
pub(crate) fn schema_id(s: &(impl Trees + ?Sized), sup: &Supportive, schema: Option<&str>) -> Result<Option<u64>> {
    let Some(name) = schema else { return Ok(Some(0)) };
    if name.is_empty() || name.len() > 255 {
        return Ok(None);
    }
    match sup.register.get(s, &sch::name_index_key(sch::NAME_SCHEMA, 0, name))? {
        Some((1, p)) => Ok(Some(u64_payload(&p)?)),
        Some(_) => Err(corrupt("schema nIDX version")),
        None => Ok(None),
    }
}

fn schema_name(s: &(impl Trees + ?Sized), sup: &Supportive, id: u64) -> Result<Option<String>> {
    if id == 0 {
        return Ok(None);
    }
    match sup.register.get(s, &sch::name_key(sch::NAME_SCHEMA, id))? {
        Some((1, p)) => Ok(Some(Name::decode(&p)?.name)),
        _ => Err(corrupt("a table names a schema with no NAME")),
    }
}

/// The table called `name` in a schema.
pub(crate) fn table_id(s: &(impl Trees + ?Sized), sup: &Supportive, schema: u64, name: &str) -> Result<Option<u32>> {
    if name.is_empty() || name.len() > 255 {
        return Ok(None);
    }
    match sup.register.get(s, &sch::name_index_key(sch::NAME_TABLE, schema, name))? {
        Some((1, p)) => Ok(Some(u32::try_from(u64_payload(&p)?).map_err(|_| corrupt("table id"))?)),
        Some(_) => Err(corrupt("table nIDX version")),
        None => Ok(None),
    }
}

/// Every name of one class under one owner, in name order, with its id.
pub(crate) fn names(s: &(impl Trees + ?Sized), sup: &Supportive, class: u8, schema: u64) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for (key, version, p) in sup.register.scan_prefix(s, &sch::name_index_prefix(class, schema))? {
        let crate::supportive::carrier::Item::Name { name, .. } = key.item else {
            return Err(corrupt("nIDX item"));
        };
        if version != 1 {
            return Err(corrupt("nIDX version"));
        }
        let name = String::from_utf8(name).map_err(|_| corrupt("nIDX name"))?;
        out.push((name, u64_payload(&p)?));
    }
    Ok(out)
}

/// A table's columns by id.
pub(crate) fn columns(s: &(impl Trees + ?Sized), sup: &Supportive, table: u32) -> Result<BTreeMap<u64, Column>> {
    let mut out = BTreeMap::new();
    for (key, version, p) in sup.register.scan_prefix(s, &sch::columns_prefix(u64::from(table)))? {
        let crate::supportive::carrier::Item::Id(id) = key.item else {
            return Err(corrupt("COLM item"));
        };
        if version != 1 {
            return Err(corrupt("COLM version"));
        }
        out.insert(id, Column::decode(&p)?);
    }
    Ok(out)
}

pub(crate) fn read_catalog(s: &(impl Trees + ?Sized), sup: &Supportive, id: CollectionId) -> Result<Option<Catalog>> {
    let t = u64::from(id.0);
    let table = match sup.register.get(s, &sch::table_key(t))? {
        Some((1, p)) => Table::decode(&p)?,
        Some(_) => return Err(corrupt("TABL version")),
        None => return Ok(None),
    };
    let name = match sup.register.get(s, &sch::name_key(sch::NAME_TABLE, t))? {
        Some((1, p)) => Name::decode(&p)?,
        _ => return Err(corrupt("a table with no NAME")),
    };
    if name.parent != table.schema {
        return Err(corrupt("table NAME and TABL disagree on the schema"));
    }
    let mut declared = Vec::new();
    let mut rules = Vec::new();
    for c in columns(s, sup, id.0)?.into_values() {
        if let Some(d) = c.declared {
            declared.push((c.name.clone(), d));
        }
        if let Some(r) = c.rule {
            let mut at = 0;
            let rule = column_rules::decode_rule(|n| {
                let out = r.get(at..at + n).ok_or_else(|| corrupt("COLM rule"))?.to_vec();
                at += n;
                Ok(out)
            })?;
            if at != r.len() {
                return Err(corrupt("COLM rule length"));
            }
            rules.push(rule);
        }
    }
    let tail = |key: crate::supportive::carrier::Key, bit: u8| -> Result<Option<Vec<u8>>> {
        if table.parts & bit == 0 {
            return Ok(None);
        }
        match sup.register.get(s, &key)? {
            Some((1, p)) => Ok(Some(p)),
            Some(_) => Err(corrupt("table entry version")),
            None => Ok(None),
        }
    };
    fn reading(p: &[u8]) -> impl FnMut(usize) -> Result<Vec<u8>> + '_ {
        let mut at = 0;
        move |n| {
            let out = p.get(at..at + n).ok_or_else(|| corrupt("Register payload too short"))?.to_vec();
            at += n;
            Ok(out)
        }
    }
    let key = tail(sch::keys_key(t), sch::TABLE_KEYS)?.map(|p| column_rules::decode_key_spec(reading(&p))).transpose()?;
    let edge = tail(sch::bind_key(t), sch::TABLE_BIND)?
        .map(|p| crate::index::graph::edge_table::EdgeTableRecord::decode(reading(&p)))
        .transpose()?;
    let graphs = tail(sch::memb_key(t), sch::TABLE_MEMB)?
        .map(|p| crate::index::graph::property_graph::decode_memberships(reading(&p)))
        .transpose()?
        .unwrap_or_default();
    let drop = match tail(sch::drop_job_key(t), sch::TABLE_DROP)? {
        Some(p) if p.len() == 10 => Some(DropState {
            phase: DropPhase::from_byte(p[0])?,
            mode: DropMode::from_byte(p[1])?,
            removed: u64::from_be_bytes(p[2..10].try_into().unwrap()),
        }),
        Some(_) => return Err(corrupt("table-drop JOBS payload")),
        None => None,
    };
    Ok(Some(Catalog {
        id,
        name: name.name,
        layout: table.layout,
        timestamps: table.timestamps,
        declared,
        rules,
        schema: schema_name(s, sup, table.schema)?,
        edge,
        key,
        graphs,
        drop,
    }))
}

/// Write the entries of `c` that differ from `old` (all of them for a new
/// table). The `nIDX` lookup is the name functions' own business.
pub(crate) fn write_catalog<W: TreesMut + Trees>(
    w: &mut W,
    sup: &mut Supportive,
    old: Option<&Catalog>,
    c: &Catalog,
) -> Result<()> {
    let t = u64::from(c.id.0);
    let schema = schema_id(w, sup, c.schema.as_deref())?
        .ok_or_else(|| invalid(format!("schema `{}` does not exist", c.schema.as_deref().unwrap_or(""))))?;
    let parts = |c: &Catalog| {
        (if c.key.is_some() { sch::TABLE_KEYS } else { 0 })
            | (if c.edge.is_some() { sch::TABLE_BIND } else { 0 })
            | (if c.graphs.is_empty() { 0 } else { sch::TABLE_MEMB })
            | (if c.drop.is_some() { sch::TABLE_DROP } else { 0 })
    };
    let table = Table { timestamps: c.timestamps, layout: c.layout, schema, parts: parts(c) };
    if old.is_none_or(|o| {
        o.timestamps != c.timestamps || o.layout != c.layout || o.schema != c.schema || parts(o) != parts(c)
    }) {
        sup.put(w, &sch::table_key(t), line(b"TABL", 1, 0), &table.encode())?;
    }
    if old.is_none_or(|o| o.name != c.name || o.schema != c.schema) {
        let name = Name { parent: schema, name: c.name.clone() };
        sup.put(w, &sch::name_key(sch::NAME_TABLE, t), line(b"NAME", 1, u32::from(sch::NAME_TABLE)), &name.encode()?)?;
    }
    if old.is_none_or(|o| o.declared != c.declared || o.rules != c.rules) {
        let mut cols = columns(w, sup, c.id.0)?;
        let mut touched = Vec::new();
        for (id, col) in cols.iter_mut() {
            if !col.live {
                continue;
            }
            let declared = c.declared.iter().find(|(n, _)| *n == col.name).map(|(_, d)| d.clone());
            let rule = match c.rules.iter().find(|(n, _)| *n == col.name) {
                Some((n, r)) => {
                    let mut b = Vec::new();
                    column_rules::encode_rule(&mut b, n, r)?;
                    Some(b)
                }
                None => None,
            };
            if col.declared != declared || col.rule != rule {
                col.declared = declared;
                col.rule = rule;
                touched.push(*id);
            }
        }
        // A declared spelling or rule for a field with no column yet (the
        // layout names every field that can carry one, so this is a new
        // table whose layout is written in the same transaction first).
        for field in c.declared.iter().map(|(n, _)| n).chain(c.rules.iter().map(|(n, _)| n)) {
            if !cols.values().any(|col| col.live && &col.name == field) {
                return Err(invalid(format!("a declared type or rule names `{field}`, which is not a column")));
            }
        }
        for id in touched {
            let col = &cols[&id];
            sup.put(w, &sch::column_key(t, id), line(b"COLM", 1, 0), &col.encode()?)?;
        }
    }
    // The tails a table may carry, each its own entry, in the 0.18 tail
    // encodings.
    let mut encoded = |present: bool, key: crate::supportive::carrier::Key, code: &[u8; 4], variant: u32, bytes: Result<Vec<u8>>| -> Result<()> {
        if present {
            sup.put(w, &key, line(code, 1, variant), &bytes?)
        } else {
            sup.delete(w, &key).map(|_| ())
        }
    };
    if old.is_none_or(|o| o.key != c.key) {
        encoded(
            c.key.is_some(),
            sch::keys_key(t),
            b"KEYS",
            0,
            c.key.as_ref().map_or(Ok(Vec::new()), |k| {
                let mut b = Vec::new();
                column_rules::encode_key_spec(&mut b, k).map(|_| b)
            }),
        )?;
    }
    if old.is_none_or(|o| o.edge != c.edge) {
        encoded(
            c.edge.is_some(),
            sch::bind_key(t),
            b"BIND",
            0,
            c.edge.as_ref().map_or(Ok(Vec::new()), |e| {
                let mut b = Vec::new();
                e.encode(&mut b).map(|_| b)
            }),
        )?;
    }
    if old.is_none_or(|o| o.graphs != c.graphs) {
        let mut b = Vec::new();
        let r = if c.graphs.is_empty() {
            Ok(b)
        } else {
            crate::index::graph::property_graph::encode_memberships(&mut b, &c.graphs).map(|_| b)
        };
        encoded(!c.graphs.is_empty(), sch::memb_key(t), b"MEMB", 0, r)?;
    }
    if old.is_none_or(|o| o.drop != c.drop) {
        let bytes = c.drop.map(|d| {
            let mut b = vec![d.phase.byte(), d.mode.byte()];
            b.extend_from_slice(&d.removed.to_be_bytes());
            b
        });
        encoded(c.drop.is_some(), sch::drop_job_key(t), b"JOBS", 3, Ok(bytes.unwrap_or_default()))?;
    }
    Ok(())
}

/// A table's row sequence (`NEXT` class 4).
pub(crate) fn sequence(s: &(impl Trees + ?Sized), sup: &Supportive, table: u32) -> Result<u64> {
    match sup.register.get(s, &sch::sequence_key(u64::from(table)))? {
        Some((1, p)) if p.len() == 8 && p != [0; 8] => u64_payload(&p),
        _ => Err(corrupt(format!("table {table} has no row sequence NEXT"))),
    }
}

/// Remove every entry of a table (its `nIDX` is the caller's).
pub(crate) fn delete_table<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, id: CollectionId) -> Result<()> {
    let t = u64::from(id.0);
    let cols: Vec<u64> = columns(w, sup, id.0)?.into_keys().collect();
    for col in cols {
        sup.delete(w, &sch::column_key(t, col))?;
    }
    for key in [
        sch::table_key(t),
        sch::name_key(sch::NAME_TABLE, t),
        sch::keys_key(t),
        sch::bind_key(t),
        sch::memb_key(t),
        sch::drop_job_key(t),
        sch::column_next_key(t),
        sch::sequence_key(t),
    ] {
        sup.delete(w, &key)?;
    }
    Ok(())
}

fn layout_parts(s: &(impl Trees + ?Sized), sup: &Supportive, id: u32) -> Result<Vec<LayoutPart>> {
    let mut parts = Vec::new();
    for part in 0..=u8::MAX {
        match sup.register.get(s, &sch::layout_key(u64::from(id), part))? {
            Some((1, p)) => parts.push(LayoutPart::decode(&p)?),
            Some(_) => return Err(corrupt("LAYT version")),
            None => break,
        }
    }
    if parts.iter().any(|p| p.table != parts[0].table) {
        return Err(corrupt("LAYT parts of different tables"));
    }
    Ok(parts)
}

/// A layout, its slots resolved to the columns' names. `columns` is the
/// table's column map when the caller has it cached.
pub(crate) fn read_layout(
    s: &(impl Trees + ?Sized),
    sup: &Supportive,
    id: u32,
    cached: Option<(u32, &BTreeMap<u64, Column>)>,
) -> Result<Option<(u32, Layout)>> {
    let parts = layout_parts(s, sup, id)?;
    let Some(first) = parts.first() else { return Ok(None) };
    let table = first.table;
    let owned;
    let cols = match cached {
        Some((t, c)) if t == table => c,
        _ => {
            owned = columns(s, sup, table)?;
            &owned
        }
    };
    let mut fields = Vec::new();
    for (col, code, dim) in parts.iter().flat_map(|p| p.slots.iter()) {
        let c = cols.get(col).ok_or_else(|| corrupt("a layout slot names a missing column"))?;
        let name = if c.live { c.name.clone() } else { super::columns::retired_name(*col) };
        fields.push((name, sch::kind_from(*code, *dim)?));
    }
    // F2: the columns added after this layout with a value for older rows.
    let absent: Vec<(String, serde_json::Value)> = cols
        .iter()
        .filter(|(id, c)| c.live && !parts.iter().any(|p| p.slots.iter().any(|s| s.0 == **id)))
        .filter_map(|(_, c)| c.missing.clone().map(|v| (c.name.clone(), v)))
        .collect();
    let absent = crate::Absent((!absent.is_empty()).then(|| std::sync::Arc::new(absent)));
    Ok(Some((table, Layout { id: u64::from(id), fields, absent })))
}

/// Write a new layout of `table`, allocating column ids for the names it
/// has not seen before.
pub(crate) fn write_layout<W: TreesMut + Trees>(
    w: &mut W,
    sup: &mut Supportive,
    table: CollectionId,
    l: &Layout,
) -> Result<()> {
    let t = u64::from(table.0);
    let mut cols = columns(w, sup, table.0)?;
    let mut next = match sup.register.get(w, &sch::column_next_key(t))? {
        Some((1, p)) => u64_payload(&p)?,
        Some(_) => return Err(corrupt("column NEXT version")),
        None => sch::FIRST_USER_COLUMN,
    };
    let start = next;
    let mut slots = Vec::with_capacity(l.fields.len());
    for (name, kind) in &l.fields {
        let id = match cols.iter().find(|(_, c)| c.live && &c.name == name) {
            Some((id, _)) => *id,
            None => {
                let id = match name.as_str() {
                    KEY_FIELD => sch::COLUMN_KEY,
                    CREATED => sch::COLUMN_CREATED,
                    UPDATED => sch::COLUMN_UPDATED,
                    _ => {
                        let id = next;
                        next = next.checked_add(1).ok_or_else(|| invalid("column ids exhausted"))?;
                        id
                    }
                };
                // An edge table's bags hold properties under their token, so
                // a token another column already holds (a renamed or retired
                // one) is never reused: the new column gets one no name can be.
                let token = if cols.values().any(|c| &c.stored_token == name) {
                    super::columns::retired_name(id)
                } else {
                    name.clone()
                };
                let col = Column { live: true, name: name.clone(), stored_token: token, declared: None, rule: None, missing: None };
                sup.put(w, &sch::column_key(t, id), line(b"COLM", 1, 0), &col.encode()?)?;
                cols.insert(id, col);
                id
            }
        };
        let (code, dim) = sch::kind_code(kind);
        slots.push((id, code, dim));
    }
    if next != start {
        sup.put(w, &sch::column_next_key(t), line(b"NEXT", 1, sch::NEXT_COLUMN as u32), &next.to_be_bytes())?;
    }
    let parts: Vec<LayoutPart> = slots
        .chunks(sch::SLOTS_PER_PART)
        .map(|c| LayoutPart { table: table.0, slots: c.to_vec() })
        .collect();
    let variant = LayoutPart::variant(&parts);
    if parts.len() > 256 {
        return Err(invalid("too many layout slots"));
    }
    for (i, part) in parts.iter().enumerate() {
        sup.put(w, &sch::layout_key(l.id, i as u8), line(b"LAYT", 1, variant), &part.encode())?;
    }
    Ok(())
}

pub(crate) fn delete_layout<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, id: u32) -> Result<()> {
    let n = layout_parts(w, sup, id)?.len();
    for part in 0..n {
        sup.delete(w, &sch::layout_key(u64::from(id), part as u8))?;
    }
    Ok(())
}

/// `nIDX` of a table: put or delete.
pub(crate) fn put_table_name<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, schema: u64, name: &str, id: CollectionId) -> Result<()> {
    sup.put(
        w,
        &sch::name_index_key(sch::NAME_TABLE, schema, name),
        line(b"nIDX", 1, 0),
        &u64::from(id.0).to_be_bytes(),
    )
}
pub(crate) fn delete_table_name<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, schema: u64, name: &str) -> Result<()> {
    sup.delete(w, &sch::name_index_key(sch::NAME_TABLE, schema, name)).map(|_| ())
}

/// A new named schema: its id, `NAME` and `nIDX`.
pub(crate) fn create_schema<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, schema: &str) -> Result<()> {
    let next = match sup.register.get(w, &sch::schema_next_key())? {
        Some((1, p)) => u64_payload(&p)?,
        Some(_) => return Err(corrupt("schema NEXT version")),
        None => 1,
    };
    let after = next.checked_add(1).ok_or_else(|| invalid("schema ids exhausted"))?;
    sup.put(w, &sch::schema_next_key(), line(b"NEXT", 1, sch::NEXT_SCHEMA as u32), &after.to_be_bytes())?;
    let name = Name { parent: 0, name: schema.to_owned() };
    sup.put(w, &sch::name_key(sch::NAME_SCHEMA, next), line(b"NAME", 1, u32::from(sch::NAME_SCHEMA)), &name.encode()?)?;
    sup.put(w, &sch::name_index_key(sch::NAME_SCHEMA, 0, schema), line(b"nIDX", 1, 0), &next.to_be_bytes())
}
pub(crate) fn rename_schema<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, from: &str, to: &str, id: u64) -> Result<()> {
    let name = Name { parent: 0, name: to.to_owned() };
    sup.put(w, &sch::name_key(sch::NAME_SCHEMA, id), line(b"NAME", 1, u32::from(sch::NAME_SCHEMA)), &name.encode()?)?;
    sup.delete(w, &sch::name_index_key(sch::NAME_SCHEMA, 0, from))?;
    sup.put(w, &sch::name_index_key(sch::NAME_SCHEMA, 0, to), line(b"nIDX", 1, 0), &id.to_be_bytes())
}
pub(crate) fn drop_schema<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, schema: &str, id: u64) -> Result<()> {
    sup.delete(w, &sch::name_key(sch::NAME_SCHEMA, id))?;
    sup.delete(w, &sch::name_index_key(sch::NAME_SCHEMA, 0, schema)).map(|_| ())
}

/// The table a running drop belongs to, if any.
pub(crate) fn dropping_table(s: &(impl Trees + ?Sized), sup: &Supportive) -> Result<Option<CollectionId>> {
    let prefix = [b'f', crate::supportive::carrier::OwnerClass::Table as u8];
    for (key, _, _) in sup.register.scan_prefix(s, &prefix)? {
        if key.kind == sch::kind(b"JOBS") && key.item == crate::supportive::carrier::Item::Id(3) {
            return Ok(Some(CollectionId(u32::try_from(key.owner_id).map_err(|_| corrupt("table id"))?)));
        }
    }
    Ok(None)
}

// ---- 2.d access paths: `INDX`, `TREE` and the index names ----------------

fn family_code(i: &catalog::IndexInfo) -> u8 {
    use catalog::IndexFamily::*;
    match i.family {
        Scalar => 0,
        Text if i.analyzer == Some(catalog::TextAnalyzer::Trigram) => 3,
        Text => 1,
        ExactVector => 4,
        QuantizedVector => 5,
        VamanaGraph => 6,
        SpatialPoint => 7,
        SpatialGeometry => 8,
    }
}
fn expression_code(i: &catalog::IndexInfo) -> u8 {
    match i.expression {
        None => 0,
        Some(catalog::IndexExpr::Lower) => 1,
        Some(catalog::IndexExpr::JsonText(_)) => 2,
    }
}

/// An index by id, its field resolved to the column's current name and its
/// tree root read from `TREE`.
pub(crate) fn read_index(s: &(impl Trees + ?Sized), sup: &Supportive, id: IndexId) -> Result<Option<IndexInfo>> {
    let p = match sup.register.get(s, &sch::index_key(id.0))? {
        Some((v, p)) if (1..=3).contains(&v) => p,
        Some(_) => return Err(corrupt("INDX version")),
        None => return Ok(None),
    };
    if p.len() < 8 {
        return Err(corrupt("INDX payload"));
    }
    let column = u64::from_be_bytes(p[..8].try_into().unwrap());
    let mut i = catalog::decode_body(&p[8..])?;
    if i.id != id {
        return Err(corrupt("INDX identity"));
    }
    let t = u64::from(i.collection.0);
    i.field = match sup.register.get(s, &sch::column_key(t, column))? {
        Some((1, c)) => Column::decode(&c)?.name,
        _ => return Err(corrupt("an index names a missing column")),
    };
    if let Some(tree) = i.tree {
        let root = match sup.register.get(s, &sch::tree_key(id.0, tree.id))? {
            Some((1, r)) if r.len() == 5 && r[4] == 0 => u32::from_be_bytes(r[..4].try_into().unwrap()),
            _ => return Err(corrupt("an index tree with no TREE")),
        };
        i.tree = Some(catalog::IndexTree { id: tree.id, root });
    }
    Ok(Some(i))
}

/// Write an index. `old` is what the Register holds, when the caller has it:
/// a root move then rewrites only the `TREE`.
pub(crate) fn write_index<W: TreesMut + Trees>(
    w: &mut W,
    sup: &mut Supportive,
    old: Option<&IndexInfo>,
    i: &IndexInfo,
) -> Result<()> {
    let without_root = |x: &IndexInfo| {
        let mut x = x.clone();
        x.tree = x.tree.map(|t| catalog::IndexTree { id: t.id, root: 0 });
        x
    };
    if old.is_none_or(|o| without_root(o) != without_root(i)) {
        let column = columns(w, sup, i.collection.0)?
            .into_iter()
            .find(|(_, c)| c.live && c.name == i.field)
            .map(|(id, _)| id)
            .ok_or_else(|| invalid(format!("index field `{}` is not a column", i.field)))?;
        let mut p = column.to_be_bytes().to_vec();
        p.extend(catalog::encode_body(&without_root(i))?);
        sup.put(w, &sch::index_key(i.id.0), sch::index_line(family_code(i), expression_code(i)), &p)?;
    }
    if let Some(tree) = i.tree {
        if old.and_then(|o| o.tree) != Some(tree) {
            let mut r = tree.root.to_be_bytes().to_vec();
            r.push(0);
            sup.put(w, &sch::tree_key(i.id.0, tree.id), line(b"TREE", 1, 0), &r)?;
        }
    }
    Ok(())
}

pub(crate) fn delete_index<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, i: &IndexInfo) -> Result<()> {
    sup.delete(w, &sch::index_key(i.id.0))?;
    if let Some(tree) = i.tree {
        sup.delete(w, &sch::tree_key(i.id.0, tree.id))?;
    }
    Ok(())
}

pub(crate) fn index_id(s: &(impl Trees + ?Sized), sup: &Supportive, table: CollectionId, name: &str) -> Result<Option<IndexId>> {
    if name.is_empty() || name.len() > 255 {
        return Ok(None);
    }
    match sup.register.get(s, &sch::index_name_key(u64::from(table.0), name))? {
        Some((1, p)) => Ok(Some(IndexId(u64_payload(&p)?))),
        Some(_) => Err(corrupt("index nIDX version")),
        None => Ok(None),
    }
}
pub(crate) fn put_index_name<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, table: CollectionId, name: &str, id: IndexId) -> Result<()> {
    sup.put(w, &sch::index_name_key(u64::from(table.0), name), line(b"nIDX", 1, 0), &id.0.to_be_bytes())
}
pub(crate) fn delete_index_name<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, table: CollectionId, name: &str) -> Result<()> {
    sup.delete(w, &sch::index_name_key(u64::from(table.0), name)).map(|_| ())
}
/// A table's indexes by name: (name, id), in name order.
pub(crate) fn index_names(s: &(impl Trees + ?Sized), sup: &Supportive, table: CollectionId) -> Result<Vec<(String, IndexId)>> {
    let mut out = Vec::new();
    for (key, version, p) in sup.register.scan_prefix(s, &sch::index_names_prefix(u64::from(table.0)))? {
        let crate::supportive::carrier::Item::Name { name, .. } = key.item else {
            return Err(corrupt("nIDX item"));
        };
        if version != 1 {
            return Err(corrupt("nIDX version"));
        }
        out.push((String::from_utf8(name).map_err(|_| corrupt("nIDX name"))?, IndexId(u64_payload(&p)?)));
    }
    Ok(out)
}
/// Every index id in the database, in id order.
pub(crate) fn index_ids(s: &(impl Trees + ?Sized), sup: &Supportive) -> Result<Vec<IndexId>> {
    let mut out = Vec::new();
    let prefix = sch::indexes_prefix();
    s.tree_scan(crate::supportive::carrier::REGISTER_TREES[0], sup.register.roots()[0], &prefix, &mut |k, _| {
        if !k.starts_with(&prefix) {
            return Ok(false);
        }
        let key = crate::supportive::carrier::Key::decode(k)?;
        if key.kind == sch::kind(b"INDX") {
            out.push(IndexId(key.owner_id));
        }
        Ok(true)
    })?;
    Ok(out)
}

// ---- 2.e graph: `GRPH`, the edge-type and context names -------------------

/// The graph header of a Register file: `GRPH` of the base graph and the
/// two name allocators. Name counts are derived: names are never removed,
/// so each is its allocator minus one.
pub(crate) fn read_graph(s: &(impl Trees + ?Sized), sup: &Supportive) -> Result<Option<crate::index::graph::GraphHeader>> {
    match sup.register.get(s, &sch::graph_key())? {
        Some((1, p)) if p == crate::index::graph::graph_flags() => {}
        Some((1, _)) => return Err(Error::Unsupported("needs a newer sekejap: graph encoding".into())),
        Some(_) => return Err(corrupt("GRPH version")),
        None => return Ok(None),
    }
    let next = |class: u64| -> Result<u64> {
        match sup.register.get(s, &sch::graph_next_key(class))? {
            Some((1, p)) => u64_payload(&p),
            Some(_) => Err(corrupt("graph NEXT version")),
            None => Ok(1),
        }
    };
    let next_type = next(sch::NEXT_EDGE_TYPE)?;
    let next_context = next(sch::NEXT_CONTEXT)?;
    if next_type == 0 || next_context == 0 {
        return Err(corrupt("graph name allocator"));
    }
    Ok(Some(crate::index::graph::GraphHeader {
        next_type,
        next_context,
        type_count: u32::try_from(next_type - 1).map_err(|_| corrupt("graph name count"))?,
        context_count: u32::try_from(next_context - 1).map_err(|_| corrupt("graph name count"))?,
    }))
}
pub(crate) fn write_graph<W: TreesMut + Trees>(
    w: &mut W,
    sup: &mut Supportive,
    old: Option<crate::index::graph::GraphHeader>,
    h: crate::index::graph::GraphHeader,
) -> Result<()> {
    if old.is_none() {
        sup.put(w, &sch::graph_key(), line(b"GRPH", 1, 0), &crate::index::graph::graph_flags())?;
    }
    if old.is_none_or(|o| o.next_type != h.next_type) {
        sup.put(w, &sch::graph_next_key(sch::NEXT_EDGE_TYPE), line(b"NEXT", 1, sch::NEXT_EDGE_TYPE as u32), &h.next_type.to_be_bytes())?;
    }
    if old.is_none_or(|o| o.next_context != h.next_context) {
        sup.put(w, &sch::graph_next_key(sch::NEXT_CONTEXT), line(b"NEXT", 1, sch::NEXT_CONTEXT as u32), &h.next_context.to_be_bytes())?;
    }
    Ok(())
}
fn graph_class(kind: u8) -> u8 {
    if kind == 0 { sch::NAME_EDGE_TYPE } else { sch::NAME_CONTEXT }
}
/// Graph name kind 0 is an edge type, 1 a context, as in 0.18.
pub(crate) fn graph_name_id(s: &(impl Trees + ?Sized), sup: &Supportive, kind: u8, name: &str) -> Result<Option<u64>> {
    match sup.register.get(s, &sch::graph_name_index_key(graph_class(kind), name))? {
        Some((1, p)) => Ok(Some(u64_payload(&p)?)),
        Some(_) => Err(corrupt("graph nIDX version")),
        None => Ok(None),
    }
}
pub(crate) fn graph_name(s: &(impl Trees + ?Sized), sup: &Supportive, kind: u8, id: u64) -> Result<Option<String>> {
    match sup.register.get(s, &sch::graph_name_key(graph_class(kind), id))? {
        Some((1, p)) => Ok(Some(Name::decode(&p)?.name)),
        Some(_) => Err(corrupt("graph NAME version")),
        None => Ok(None),
    }
}
pub(crate) fn put_graph_name<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, kind: u8, id: u64, name: &str) -> Result<()> {
    let class = graph_class(kind);
    let n = Name { parent: 0, name: name.to_owned() };
    sup.put(w, &sch::graph_name_key(class, id), line(b"NAME", 1, u32::from(class)), &n.encode()?)?;
    sup.put(w, &sch::graph_name_index_key(class, name), line(b"nIDX", 1, 0), &id.to_be_bytes())
}
/// Free an edge-type or context name for reuse. Its `NAME` stays, so the
/// retired id still reads back its name; only the lookup goes.
pub(crate) fn retire_graph_name<W: TreesMut + Trees>(w: &mut W, sup: &mut Supportive, kind: u8, name: &str) -> Result<()> {
    sup.delete(w, &sch::graph_name_index_key(graph_class(kind), name)).map(|_| ())
}
/// Every name of one graph kind, in name order.
pub(crate) fn graph_name_list(s: &(impl Trees + ?Sized), sup: &Supportive, kind: u8) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for (key, version, p) in sup.register.scan_prefix(s, &sch::graph_name_index_prefix(graph_class(kind)))? {
        let crate::supportive::carrier::Item::Name { name, .. } = key.item else {
            return Err(corrupt("nIDX item"));
        };
        if version != 1 {
            return Err(corrupt("nIDX version"));
        }
        out.push((String::from_utf8(name).map_err(|_| corrupt("nIDX name"))?, u64_payload(&p)?));
    }
    Ok(out)
}
