//! F1, column identity on rows (`docs/core/SUPPORTIVE.md` 2.c): on a
//! Register file a column is an id, and its name is a label.
//!
//! * `RENAME COLUMN` writes one `COLM`. Every layout names the column by id,
//!   so every row -- whatever layout it was written under -- reads back under
//!   the new name, and every index over the column follows it. No row, layout
//!   or index key is rewritten.
//! * `DROP COLUMN` retires the id and publishes a layout without it. Rows
//!   written before keep their bytes; the retired slot reads as nothing, so
//!   a column added later under the same name is a new id and never shows an
//!   old value.
//!
//! A 0.18-format file has no column ids: both refuse there by name.

use super::*;
use crate::supportive::schema::{self as sch, line};

/// The name a retired column's slot resolves to. A NUL cannot be written in
/// a column name, so no statement can name it, and every whole-row decode
/// drops it.
pub(crate) fn retired_name(id: u64) -> String {
    format!("\u{0}{id}")
}
pub(crate) fn is_retired(name: &str) -> bool {
    name.starts_with('\u{0}')
}

/// F1 on edges. An edge table's properties live in each edge's bag under a
/// STORED TOKEN: the column's name when it was added. A rename keeps the
/// token; a dropped column's token is retired; a column added under a name
/// some other column's token already holds gets a token no name can be. This
/// map turns a stored bag into names and back. A table that was never
/// renamed or dropped has no map, and its bags pass through untouched.
#[derive(Debug, Default)]
pub(crate) struct BagMap {
    to_name: std::collections::HashMap<String, String>,
    to_token: std::collections::HashMap<String, String>,
    retired: std::collections::HashSet<String>,
    /// F2 on edges: what an edge written before a column existed reads for
    /// it, when the column was added with a DEFAULT.
    defaults: Vec<(String, Value)>,
}
impl BagMap {
    fn of(columns: &std::collections::BTreeMap<u64, sch::Column>) -> Option<Self> {
        let mut m = Self::default();
        for c in columns.values() {
            if !c.live {
                m.retired.insert(c.stored_token.clone());
                continue;
            }
            if c.stored_token != c.name {
                m.to_name.insert(c.stored_token.clone(), c.name.clone());
                m.to_token.insert(c.name.clone(), c.stored_token.clone());
            }
            if let Some(v) = &c.missing {
                m.defaults.push((c.name.clone(), v.clone()));
            }
        }
        (!m.to_name.is_empty() || !m.retired.is_empty() || !m.defaults.is_empty()).then_some(m)
    }
    /// A stored bag, read under the columns' names.
    pub(crate) fn names(&self, bag: &Value) -> Value {
        let Some(object) = bag.as_object() else { return bag.clone() };
        let mut out = serde_json::Map::with_capacity(object.len());
        for (token, value) in object {
            if self.retired.contains(token) && !self.to_name.contains_key(token) {
                continue;
            }
            let name = self.to_name.get(token).unwrap_or(token);
            out.insert(name.clone(), value.clone());
        }
        for (name, value) in &self.defaults {
            out.entry(name.clone()).or_insert_with(|| value.clone());
        }
        Value::Object(out)
    }
    /// A bag keyed by names, as it is stored.
    pub(crate) fn tokens(&self, bag: &Value) -> Value {
        let Some(object) = bag.as_object() else { return bag.clone() };
        let mut out = serde_json::Map::with_capacity(object.len());
        for (name, value) in object {
            let token = self.to_token.get(name).unwrap_or(name);
            out.insert(token.clone(), value.clone());
        }
        Value::Object(out)
    }
}

impl Database {
    /// The token map of an edge type's table, when it has one. Worked out
    /// once per type and kept until the catalog changes.
    pub(crate) fn bag_map(&self, t: EdgeTypeId) -> Result<Option<Arc<BagMap>>> {
        if self.supportive.is_none() {
            return Ok(None);
        }
        if let Some(found) = self.bag_maps.borrow().as_ref().and_then(|m| m.get(&t)) {
            return Ok(found.clone());
        }
        let table = self.edge_table_types()?.get(&t).copied();
        self.bag_map_of(t, table)
    }

    /// [`Self::bag_map`] when the caller already knows the type's table.
    pub(crate) fn bag_map_of(&self, t: EdgeTypeId, table: Option<CollectionId>) -> Result<Option<Arc<BagMap>>> {
        if self.supportive.is_none() {
            return Ok(None);
        }
        if let Some(found) = self.bag_maps.borrow().as_ref().and_then(|m| m.get(&t)) {
            return Ok(found.clone());
        }
        let sup = self.supportive.as_ref().expect("checked above");
        let map = match table {
            Some(c) => BagMap::of(&register_catalog::columns(self.store()?, sup, c.0)?).map(Arc::new),
            None => None,
        };
        self.bag_maps.borrow_mut().get_or_insert_with(BTreeMap::new).insert(t, map.clone());
        Ok(map)
    }

    /// A dropped table's reserved row ids go with it.
    pub(crate) fn row_id_blocks_forget(&mut self, c: CollectionId) {
        self.row_id_blocks.remove(&c);
        self.row_id_pending.remove(&c);
    }

    /// Whether columns have ids in this file: a Register file.
    pub fn has_column_ids(&self) -> bool {
        self.supportive.is_some()
    }

    fn column_ids_required(&self, what: &str) -> Result<()> {
        if self.supportive.is_none() {
            return Err(Error::Unsupported(format!(
                "{what} needs column ids, which a 0.18-format file does not have; upgrade it with sekejap-upgrade"
            )));
        }
        Ok(())
    }

    fn live_column(&self, c: CollectionId, name: &str) -> Result<(u64, sch::Column)> {
        let sup = self.supportive.as_ref().expect("checked by the caller");
        register_catalog::columns(self.store()?, sup, c.0)?
            .into_iter()
            .find(|(_, col)| col.live && col.name == name)
            .ok_or_else(|| invalid(format!("no column `{name}`")))
    }

    fn catalog_changed(&self) {
        *self.catalog_cache.borrow_mut() = None;
        *self.layout_cache.borrow_mut() = None;
        *self.bag_maps.borrow_mut() = None;
        self.index_descriptors_changed();
    }

    /// `ALTER TABLE t RENAME COLUMN from TO to`: one `COLM` write.
    pub fn rename_column(&mut self, c: CollectionId, from: &str, to: &str) -> Result<()> {
        self.ready_write()?;
        self.column_ids_required("RENAME COLUMN on a table with rows")?;
        if to.is_empty() || to.len() > 255 || is_retired(to) || reserved(to) {
            return Err(invalid(format!("`{to}` cannot be a column name")));
        }
        if matches!(from, KEY_FIELD | CREATED | UPDATED) || matches!(to, CREATED | UPDATED) {
            return Err(invalid("a managed column cannot be renamed"));
        }
        self.catalog(c)?;
        let (id, mut col) = self.live_column(c, from)?;
        if self.live_column(c, to).is_ok() {
            return Err(Error::AlreadyExists);
        }
        col.name = to.to_owned();
        if let Some(bytes) = col.rule.take() {
            let mut at = 0;
            let (_, rule) = column_rules::decode_rule(|n| {
                let out = bytes.get(at..at + n).ok_or_else(|| corrupt("COLM rule"))?.to_vec();
                at += n;
                Ok(out)
            })?;
            let mut b = Vec::new();
            column_rules::encode_rule(&mut b, to, &rule)?;
            col.rule = Some(b);
        }
        self.user_write()?;
        let result = (|| {
            let bytes = col.encode()?;
            self.entry_put(&sch::column_key(u64::from(c.0), id), line(b"COLM", 1, 0), &[], &bytes)?;
            self.catalog_changed();
            Ok(())
        })();
        self.finish(result)
    }

    /// F2: the value rows written before `column` existed read for it -- what
    /// `ADD COLUMN column ... DEFAULT value` promises them, as PostgreSQL
    /// shows it. One column record; no row is rewritten. Called in the same
    /// transaction as the layout that adds the column.
    pub fn set_missing_default(&mut self, c: CollectionId, column: &str, value: Value) -> Result<()> {
        self.ready_write()?;
        self.column_ids_required("ADD COLUMN ... DEFAULT on a table with rows")?;
        self.catalog(c)?;
        let (id, mut col) = self.live_column(c, column)?;
        col.missing = Some(value);
        self.user_write()?;
        let result = (|| {
            let bytes = col.encode()?;
            self.entry_put(&sch::column_key(u64::from(c.0), id), line(b"COLM", 1, 0), &[], &bytes)?;
            self.catalog_changed();
            Ok(())
        })();
        self.finish(result)
    }

    /// `ALTER TABLE t DROP COLUMN column`: the id is retired and a layout
    /// without it is published, in one transaction. Indexes over the column
    /// must be dropped first. Returns the new layout id.
    pub fn drop_column(&mut self, c: CollectionId, column: &str) -> Result<u64> {
        self.ready_write()?;
        self.column_ids_required("DROP COLUMN")?;
        if matches!(column, KEY_FIELD | CREATED | UPDATED) {
            return Err(invalid("a managed column cannot be dropped"));
        }
        let info = self.collection_info(c)?;
        if !info.layout.fields.iter().any(|(n, _)| n == column) {
            return Err(invalid(format!("no column `{column}`")));
        }
        if let Some(i) = self.list_indexes(c)?.into_iter().find(|i| i.field == column) {
            return Err(invalid(format!(
                "DROP COLUMN {column}: the index `{}` is over it; drop the index first",
                i.name
            )));
        }
        let (id, mut col) = self.live_column(c, column)?;
        col.live = false;
        col.declared = None;
        col.rule = None;
        col.missing = None;
        let fields: Vec<(String, Kind)> =
            info.layout.fields.into_iter().filter(|(n, _)| n != column).collect();
        let declared = info.declared.into_iter().filter(|(n, _)| n != column).collect();
        let rules = info.rules.into_iter().filter(|(n, _)| n != column).collect();
        self.user_write()?;
        let retired = (|| {
            let bytes = col.encode()?;
            self.entry_put(&sch::column_key(u64::from(c.0), id), line(b"COLM", 1, 0), &[], &bytes)?;
            self.catalog_changed();
            Ok(())
        })();
        self.finish(retired)?;
        self.alter_collection_rules(c, fields, declared, rules)
    }
}

/// N7 and the rest of F2: names that move, and column rules that change
/// after the column exists. Every one is a catalog write: no row, layout or
/// index key moves.
impl Database {
    /// `ALTER COLUMN c SET/DROP DEFAULT`, `SET/DROP NOT NULL`: the table's
    /// column rules, replaced whole. A NOT NULL the rules did not have is
    /// checked against every row first, as PostgreSQL checks it, and refused
    /// naming the column when a row holds no value.
    pub fn set_column_rules(&mut self, c: CollectionId, rules: Vec<(String, ColumnRule)>) -> Result<()> {
        self.ready_write()?;
        let mut cat = self.catalog(c)?;
        let info = self.collection_info(c)?;
        column_rules::check_rules(&info.layout.fields, &rules)?;
        for (field, rule) in &rules {
            let had = cat.rules.iter().any(|(f, r)| f == field && r.not_null);
            if rule.not_null && !had {
                self.refuse_nulls(c, &cat.name, field)?;
            }
        }
        cat.rules = rules;
        self.user_write()?;
        let result = (|| {
            if !cat.rules.is_empty() {
                self.enable_logical_feature(column_rules::COLUMN_RULES_FEATURE)?;
            }
            if column_rules::has_constant(&cat.rules) {
                self.enable_logical_feature(column_rules::CONSTANT_DEFAULT_FEATURE)?;
            }
            self.persist_catalog(&cat)?;
            self.catalog_changed();
            Ok(())
        })();
        self.finish(result)
    }

    fn refuse_nulls(&self, c: CollectionId, table: &str, field: &str) -> Result<()> {
        for entity in self.scan(c, None)? {
            let entity = entity?;
            if entity.document.get(field).is_none_or(Value::is_null) {
                return Err(Error::Constraint {
                    sqlstate: "23502",
                    message: format!("column `{field}` of `{table}` contains null values (row `{}`)", entity.key),
                });
            }
        }
        Ok(())
    }

    /// `ALTER TABLE t SET SCHEMA s`: the table's name moves to another
    /// schema. Its id, rows, indexes and edges stay.
    pub fn move_collection(&mut self, c: CollectionId, schema: &str) -> Result<()> {
        self.ready_write()?;
        let mut cat = self.catalog(c)?;
        let target = (schema != PUBLIC_SCHEMA).then(|| schema.to_owned());
        if target == cat.schema {
            return Ok(());
        }
        if !self.schema_exists(schema)? {
            return Err(invalid(format!("schema `{schema}` does not exist")));
        }
        if self.collection_in(schema, &cat.name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        self.user_write()?;
        let old = cat.schema.clone();
        cat.schema = target.clone();
        let result = (|| {
            if target.is_some() {
                self.enable_logical_feature(SCHEMA_FEATURE)?;
            }
            self.persist_catalog(&cat)?;
            self.delete_table_name(old.as_deref(), &cat.name)?;
            self.put_table_name(target.as_deref(), &cat.name, c)?;
            self.catalog_changed();
            Ok(())
        })();
        self.finish(result)
    }

    /// `ALTER INDEX i RENAME TO j`.
    pub fn rename_index(&mut self, id: IndexId, to: &str) -> Result<()> {
        self.ready_write()?;
        if to.is_empty() || to.len() > 128 {
            return Err(invalid("index name requires 1..128 UTF-8 bytes"));
        }
        let mut i = self.index_info(id)?;
        if i.name == to {
            return Ok(());
        }
        if self.index_name_taken(i.collection, to)? {
            return Err(Error::AlreadyExists);
        }
        self.user_write()?;
        let old = std::mem::replace(&mut i.name, to.to_owned());
        let result = (|| {
            self.save_index(&i)?;
            self.register_index_name(i.collection, &old, None, false)?;
            self.register_index_name(i.collection, to, Some(id), false)?;
            self.index_descriptors_changed();
            Ok(())
        })();
        self.finish(result)
    }

    /// `ALTER SCHEMA s RENAME TO t`: one `NAME` and its lookup. A 0.18-format
    /// file names a table's schema in every table's record, so it refuses.
    pub fn rename_schema(&mut self, from: &str, to: &str) -> Result<()> {
        self.ready_write()?;
        self.column_ids_required("ALTER SCHEMA ... RENAME")?;
        check_schema_name(to)?;
        let sup = self.supportive.as_ref().expect("checked above");
        let store = self.store()?;
        let id = register_catalog::schema_id(store, sup, Some(from))?
            .filter(|_| from != PUBLIC_SCHEMA)
            .ok_or_else(|| invalid(format!("schema `{from}` does not exist")))?;
        if self.schema_exists(to)? {
            return Err(Error::AlreadyExists);
        }
        self.user_write()?;
        let result = (|| {
            let mut sup = self.supportive.take().expect("checked above");
            let r = self.writer().and_then(|w| register_catalog::rename_schema(w, &mut sup, from, to, id));
            self.supportive = Some(sup);
            r?;
            self.catalog_changed();
            Ok(())
        })();
        self.finish(result)
    }
}

#[cfg(test)]
#[path = "columns_tests.rs"]
mod columns_tests;
