use super::*;

impl Compiler<'_> {
    /// `DROP TABLE`. `Ok(None)` is `IF EXISTS` on a name that is not there.
    pub(super) fn drop_table(
        &mut self,
        table: &str,
        if_exists: bool,
        cascade: bool,
    ) -> SqlResult2<Option<WritePlan>> {
        let found = self.db.collection(table).map_err(SqlError::from);
        let collection = match found {
            Ok(Some(id)) => id,
            Ok(None) if if_exists => return Ok(None),
            Ok(None) => {
                return Err(SqlError::engine(format!("no collection named `{table}`")))
            }
            Err(e) => return Err(e),
        };
        Ok(Some(WritePlan::DropTable {
            collection,
            name: table.to_owned(),
            mode: if cascade {
                DropMode::Cascade
            } else {
                DropMode::Restrict
            },
        }))
    }

    /// `EXPLAIN DROP TABLE`. The one EXPLAIN that does not run: it prints the
    /// phases, the bound each one honours and what the collection holds, and
    /// leaves the collection there.
    pub(super) fn explain_drop_table(
        &mut self,
        table: &str,
        if_exists: bool,
        cascade: bool,
    ) -> SqlResult2<String> {
        let Some(plan) = self.drop_table(table, if_exists, cascade)? else {
            return Ok(format!(
                "drop: nothing -- IF EXISTS and no collection named `{table}`
"
            ));
        };
        let WritePlan::DropTable { collection, mode, .. } = plan else {
            unreachable!("drop_table builds only a DropTable plan")
        };
        let indexes = self.db.list_indexes(collection).map_err(SqlError::from)?;
        let mut out = String::new();
        out.push_str(&format!(
            "statement: DROP TABLE {table} {}
",
            match mode {
                DropMode::Cascade => "CASCADE",
                DropMode::Restrict => "RESTRICT (the default)",
            }
        ));
        out.push_str(
            "note:  this EXPLAIN does not run its statement. Every other EXPLAIN here runs, because a plan printed without running says nothing about the counters; running a DROP would be the drop.
",
        );
        out.push_str(&format!(
            "mark:  begin_drop_collection publishes DROPPING in the catalog descriptor and commits it before one entry is removed (Law 3); {} refuses while any graph edge in any context references a row of `{table}`, naming those contexts
",
            match mode {
                DropMode::Cascade => "CASCADE does not refuse -- RESTRICT",
                DropMode::Restrict => "RESTRICT",
            }
        ));
        out.push_str("phases:
");
        for phase in [
            DropPhase::Indexes,
            DropPhase::Sidecars,
            DropPhase::Rows,
            DropPhase::Mappings,
            DropPhase::Descriptor,
        ] {
            let detail = match phase {
                DropPhase::Indexes => format!(
                    "{} index(es): {}",
                    indexes.len(),
                    if indexes.is_empty() {
                        "none".to_owned()
                    } else {
                        indexes
                            .iter()
                            .map(|i| i.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ),
                DropPhase::Sidecars => "prefix 0x60 | collection -- vector cells".to_owned(),
                DropPhase::Rows => format!(
                    "prefix 0x40 | collection -- primary rows{}",
                    if mode == DropMode::Cascade {
                        ", each row's incident edges first through cascade_graph_delete (at most 256 per row)"
                    } else {
                        ""
                    }
                ),
                DropPhase::Mappings => {
                    "prefix 0x20 | collection -- the external-key mapping".to_owned()
                }
                DropPhase::Descriptor => {
                    "name, catalog replicas, sequence replicas, layout replicas -- after a range probe proves every keyspace above is empty".to_owned()
                }
            };
            out.push_str(&format!("  {} -- {detail}
", phase.name()));
        }
        out.push_str(&format!(
            "bound: drop_collection_step(id, budget) removes at most `budget` entries per step, budget in 1..={}; the committed cursor is the phase byte in the descriptor plus the surviving keys, so a crash resumes without a scan
",
            sekejap_core::collections::MAX_DROP_BATCH
        ));
        Ok(out)
    }

    pub(super) fn create_table(&mut self, table: String, columns: Vec<ColumnDef>) -> SqlResult2<WritePlan> {
        let mut fields = Vec::with_capacity(columns.len());
        let mut declared: Vec<(String, String)> = Vec::new();
        let mut rules: Vec<(String, ColumnRule)> = Vec::new();
        let mut keys = 0usize;
        for column in &columns {
            if column.name.starts_with('_') {
                return Err(SqlError::unsupported(format!(
                    "column `{}`: names beginning with `_` are reserved (`_id`, `_key`, `_collection`)",
                    column.name
                )));
            }
            if column.primary_key {
                keys += 1;
                if !matches!(column.kind, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "PRIMARY KEY on `{}`: an external key is a string, so the primary-key column is TEXT",
                        column.name
                    )));
                }
            }
            if column.declared == "TIMESTAMPTZ" || column.declared == "DATE" {
                self.notices.push(format!(
                    "`{}` is declared {} and stored as Kind::Int: UTC microseconds, no time-zone storage (QL_CONTRACT §5 deviation 8)",
                    column.name, column.declared
                ));
            }
            if functions::is_time_type(&column.declared) {
                declared.push((column.name.clone(), column.declared.clone()));
            }
            if let Some(rule) = &column.rule {
                self.note_rule(&column.name, &column.declared, rule);
                rules.push((column.name.clone(), rule.clone()));
            }
            fields.push((column.name.clone(), column.kind.clone()));
        }
        if keys > 1 {
            return Err(SqlError::unsupported(
                "two PRIMARY KEY columns: a row has one external key",
            ));
        }
        if keys == 1 {
            self.notices.push(format!(
                "PRIMARY KEY on `{table}`: the column is stored as a declared field AND supplies the external key `Database::put` maps, so the key is held twice (battle50k deviation 13)"
            ));
        }
        Ok(WritePlan::CreateTable {
            name: table,
            fields,
            declared,
            rules,
        })
    }

    /// What a COLUMN RULE costs and what it does not say, once per column.
    fn note_rule(&mut self, column: &str, declared: &str, rule: &ColumnRule) {
        match &rule.default {
            None => {}
            Some(DefaultValue::Now) => {
                if !functions::is_time_type(declared) {
                    self.notices.push(format!(
                        "DEFAULT now() on `{column}`, declared {declared}: the stored value is UTC microseconds and prints back as the integer it is, because only a declared TIMESTAMPTZ/DATE prints as ISO-8601 (QL_CONTRACT §4.2)"
                    ));
                }
                self.notices.push(format!(
                    "DEFAULT now() on `{column}`: ONE clock read per row, taken when the row is assembled -- not at prepare time, so two rows of one INSERT can differ"
                ));
            }
            Some(DefaultValue::Uuid4) => self.notices.push(format!(
                "DEFAULT uuid4() on `{column}`: sixteen bytes from the operating system per row, RFC 4122 version 4 -- a value the database chose, so an INSERT that wants its own writes it"
            )),
            Some(DefaultValue::Uuid5 { .. }) => self.notices.push(format!(
                "DEFAULT uuid5(...) on `{column}`: RFC 4122 version 5 over a FIXED namespace and name, so every row that takes the default takes the SAME uuid -- it is deterministic, not unique"
            )),
        }
        if rule.not_null {
            self.notices.push(format!(
                "NOT NULL on `{column}`: checked when the row is assembled; e4 distinguishes MISSING from NULL and refuses both, naming the column"
            ));
        }
    }

    // ── ALTER TABLE (QL_CONTRACT §2) ─────────────────────────────────────

    /// `ALTER TABLE t <action>`, compiled against the catalog as it stands.
    pub(super) fn alter_table(
        &mut self,
        table: &str,
        action: &AlterAction,
    ) -> SqlResult2<WritePlan> {
        let collection = collection(self.db, table)?;
        let info = self.db.collection_info(collection).map_err(SqlError::from)?;
        let indexes = self.db.list_indexes(collection).map_err(SqlError::from)?;
        let over = |column: &str| -> Vec<(IndexId, String)> {
            indexes
                .iter()
                .filter(|i| i.field == column)
                .map(|i| (i.id, i.name.clone()))
                .collect()
        };
        let named = |column: &str| -> SqlResult2<()> {
            if info.layout.fields.iter().any(|(n, _)| n == column) {
                return Ok(());
            }
            Err(SqlError::engine(format!(
                "no column `{column}` in `{table}`"
            )))
        };
        let mut fields = info.layout.fields.clone();
        let mut declared = info.declared.clone();
        let mut rules = info.rules.clone();
        let mut drop_indexes = Vec::new();
        match action {
            AlterAction::RenameTable { to } => {
                if self.db.collection(to).map_err(SqlError::from)?.is_some() {
                    return Err(SqlError::engine(format!(
                        "ALTER TABLE {table} RENAME TO {to}: a collection named `{to}` already exists"
                    )));
                }
                return Ok(WritePlan::AlterTable {
                    collection,
                    table: table.to_owned(),
                    action: CompiledAlter::Rename { to: to.clone() },
                });
            }
            AlterAction::AddColumn(column) => {
                if column.name.starts_with('_') {
                    return Err(SqlError::unsupported(format!(
                        "column `{}`: names beginning with `_` are reserved (`_id`, `_key`, `_collection`)",
                        column.name
                    )));
                }
                if fields.iter().any(|(n, _)| n == &column.name) {
                    return Err(SqlError::engine(format!(
                        "ADD COLUMN {}: `{table}` already has that column",
                        column.name
                    )));
                }
                if column.primary_key {
                    return Err(SqlError::unsupported(
                        "ADD COLUMN ... PRIMARY KEY: the external key is chosen when a row is put and is not a column the catalog can promote afterwards",
                    ));
                }
                if let Some(rule) = &column.rule {
                    if rule.not_null && rule.default.is_none() && self.any_row(collection)? {
                        return Err(SqlError::Refused {
                            keyword: "ADD COLUMN ... NOT NULL".to_owned(),
                            tier: Tier::Three,
                            reason: "QL_CONTRACT §2: every existing row would read MISSING for the new column, so the constraint is false the moment it is recorded. Add the column with a DEFAULT, or add it nullable and fill it.",
                        });
                    }
                    self.note_rule(&column.name, &column.declared, rule);
                    rules.push((column.name.clone(), rule.clone()));
                }
                if functions::is_time_type(&column.declared) {
                    declared.push((column.name.clone(), column.declared.clone()));
                }
                fields.push((column.name.clone(), column.kind.clone()));
                self.notices.push(format!(
                    "ADD COLUMN {}: every row written before this commit reads MISSING for it, which is distinct from NULL (QL_CONTRACT §2)",
                    column.name
                ));
            }
            AlterAction::DropColumn { column, if_exists } => {
                if !fields.iter().any(|(n, _)| n == column) {
                    if *if_exists {
                        return Ok(WritePlan::Notice(format!(
                            "ALTER TABLE {table} DROP COLUMN IF EXISTS {column}: no such column"
                        )));
                    }
                    named(column)?;
                }
                drop_indexes = over(column);
                if !drop_indexes.is_empty() {
                    self.notices.push(format!(
                        "DROP COLUMN {column}: its index(es) {} are dropped with it, each by the ordinary bounded drop, committed before the layout is repointed",
                        drop_indexes
                            .iter()
                            .map(|(_, name)| name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                fields.retain(|(n, _)| n != column);
                declared.retain(|(n, _)| n != column);
                rules.retain(|(n, _)| n != column);
                self.notices.push(format!(
                    "DROP COLUMN {column}: no row is rewritten -- a row decodes under the immutable layout it was written with, and the new layout is what projection and predicates see"
                ));
            }
            AlterAction::RenameColumn { from, to } => {
                named(from)?;
                if fields.iter().any(|(n, _)| n == to) {
                    return Err(SqlError::engine(format!(
                        "RENAME COLUMN {from} TO {to}: `{table}` already has a column `{to}`"
                    )));
                }
                if self.any_row(collection)? {
                    return Err(SqlError::Refused {
                        keyword: "RENAME COLUMN".to_owned(),
                        tier: Tier::Three,
                        reason: "QL_CONTRACT §2: a dense row decodes under the IMMUTABLE layout it was written with, and that layout carries the old name, so every existing row would read MISSING under the new one. Renaming a populated column needs every row rewritten and there is no bounded resumable rewrite atomic. On an empty collection it is the ordinary descriptor rewrite.",
                    });
                }
                let held = over(from);
                if let Some((_, index)) = held.first() {
                    return Err(SqlError::unsupported(format!(
                        "RENAME COLUMN {from}: the index `{index}` names `{from}` in its own descriptor, and there is no atomic that rewrites an index descriptor's field; DROP INDEX {index} first and create it again on `{to}`"
                    )));
                }
                let rename = |name: &mut String| {
                    if name == from {
                        name.clone_from(to);
                    }
                };
                fields.iter_mut().for_each(|(n, _)| rename(n));
                declared.iter_mut().for_each(|(n, _)| rename(n));
                rules.iter_mut().for_each(|(n, _)| rename(n));
                self.notices.push(format!(
                    "RENAME COLUMN {from} TO {to}: the DECLARED type and the COLUMN RULE follow the column; rows written before this commit still carry the OLD name in their own layout and read back under the new one"
                ));
            }
            AlterAction::ColumnType {
                column,
                kind,
                declared: spelling,
            } => {
                named(column)?;
                let was = fields
                    .iter()
                    .find(|(n, _)| n == column)
                    .map(|(_, k)| k.clone())
                    .expect("named() proved the column is there");
                if &was != kind {
                    return Err(SqlError::Refused {
                        keyword: format!(
                            "ALTER COLUMN {column} TYPE {spelling} ({was:?} -> {kind:?})"
                        ),
                        tier: Tier::Three,
                        reason: "QL_CONTRACT §2: a Kind change rewrites every row and re-encodes every scalar index key -- work proportional to the collection, and no bounded resumable rewrite atomic exists. Within one Kind (INT <-> BIGINT, REAL <-> DOUBLE PRECISION) it is Tier 1 and changes the declared spelling only.",
                    });
                }
                declared.retain(|(n, _)| n != column);
                if functions::is_time_type(spelling) {
                    declared.push((column.clone(), spelling.clone()));
                }
                self.notices.push(format!(
                    "ALTER COLUMN {column} TYPE {spelling}: `{was:?}` is unchanged, so no row byte and no index key moves; what changes is the DECLARED spelling in the descriptor"
                ));
            }
        }
        Ok(WritePlan::AlterTable {
            collection,
            table: table.to_owned(),
            action: CompiledAlter::Layout {
                fields,
                declared,
                rules,
                drop_indexes,
            },
        })
    }

    /// Whether the collection holds at least one row. One bounded probe --
    /// the first key of the row keyspace -- not a count.
    fn any_row(&self, collection: CollectionId) -> SqlResult2<bool> {
        Ok(self
            .db
            .scan(collection, None)
            .map_err(SqlError::from)?
            .next()
            .transpose()
            .map_err(SqlError::from)?
            .is_some())
    }

    /// `EXPLAIN ALTER TABLE ...`. Like `EXPLAIN DROP TABLE` it does NOT run
    /// its statement: a catalog rewrite performed to describe itself is the
    /// rewrite. It prints the layout id the commit would write and what the
    /// new descriptor carries.
    pub(super) fn explain_alter_table(
        &mut self,
        table: &str,
        action: &AlterAction,
    ) -> SqlResult2<String> {
        let plan = self.alter_table(table, action)?;
        let mut out = String::new();
        out.push_str(&format!(
            "statement: ALTER TABLE {table} {}
",
            action.written()
        ));
        out.push_str(
            "note:  this EXPLAIN does not run its statement. A catalog rewrite performed in order to describe itself is the rewrite.
",
        );
        match plan {
            WritePlan::Notice(text) => {
                out.push_str(&format!("nothing: {text}
"));
                return Ok(out);
            }
            WritePlan::AlterTable {
                collection,
                action: CompiledAlter::Rename { to },
                ..
            } => {
                out.push_str(&format!(
                    "rewrite: rename_collection -- one name record and the name inside the catalog descriptor, in one commit
"
                ));
                out.push_str(&format!(
                    "carries: collection id {} is unchanged, so every edge, index, layout, sequence and external-key mapping is untouched; the new name is `{to}`
",
                    collection.0
                ));
                out.push_str("layout: unchanged -- a rename writes no layout
");
            }
            WritePlan::AlterTable {
                action:
                    CompiledAlter::Layout {
                        fields,
                        declared,
                        rules,
                        drop_indexes,
                    },
                ..
            } => {
                let next = sekejap_core::internal::next_layout_id(self.db)
                    .map_err(SqlError::from)?;
                out.push_str(&format!(
                    "rewrite: alter_collection -- one new immutable Layout and a repointed catalog in one commit, O(fields); no row is rewritten
"
                ));
                out.push_str(&format!("layout: new layout id {next}
"));
                out.push_str(&format!(
                    "fields: {}
",
                    fields
                        .iter()
                        .map(|(n, k)| format!("{n} {k:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                out.push_str(&format!(
                    "carries: {} declared type(s) [{}]; {} column rule(s) [{}]
",
                    declared.len(),
                    declared
                        .iter()
                        .map(|(n, d)| format!("{n} {d}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    rules.len(),
                    rules
                        .iter()
                        .map(|(n, r)| format!("{n} {}", written_rule(r)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                out.push_str(&format!(
                    "indexes: {}
",
                    if drop_indexes.is_empty() {
                        "none dropped".to_owned()
                    } else {
                        format!(
                            "dropped with the column: {}",
                            drop_indexes
                                .iter()
                                .map(|(_, name)| name.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ));
            }
            _ => unreachable!("alter_table builds an AlterTable plan or a Notice"),
        }
        for notice in std::mem::take(self.notices) {
            out.push_str(&format!("notice: {notice}
"));
        }
        Ok(out)
    }

    pub(super) fn create_index(
        &mut self,
        name: String,
        table: &str,
        method: IndexMethod,
    ) -> SqlResult2<WritePlan> {
        let c = collection(self.db, table)?;
        let method = match method {
            IndexMethod::Btree(field) => {
                self.kind_of(c, &field)?;
                CompiledIndex::Scalar {
                    field,
                    unique: false,
                }
            }
            IndexMethod::LowerBtree(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "lower({field}): the expression QL_CONTRACT §4.1 names folds a TEXT column"
                    )));
                }
                self.notices.push(format!(
                    "an expression index over lower({field}) stores the FOLDED value: `{field} = 'X'` still needs the plain index over `{field}`, and `lower({field}) = 'x'` needs this one"
                ));
                CompiledIndex::LowerScalar { field }
            }
            IndexMethod::Gin(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Text) {
                    return Err(SqlError::unsupported(format!(
                        "gin(to_tsvector('simple', {field})): a text index spans one declared TEXT field (battle50k deviation 1)"
                    )));
                }
                CompiledIndex::Text { field }
            }
            IndexMethod::Gist(field) => match self.kind_of(c, &field)? {
                Kind::Point => CompiledIndex::Point { field },
                Kind::Geo => CompiledIndex::Geometry { field },
                other => {
                    return Err(SqlError::unsupported(format!(
                        "gist({field}): `{field}` is {other:?}; the spatial families index a Point or a Geo column"
                    )))
                }
            },
            IndexMethod::Exact(field) => {
                if !matches!(self.kind_of(c, &field)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "exact({field}): the exact family indexes a VECTOR column"
                    )));
                }
                CompiledIndex::ExactVector { field }
            }
            IndexMethod::Quantized { column, alias } => {
                if !matches!(self.kind_of(c, &column)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "quantized({column}): the quantized family indexes a VECTOR column"
                    )));
                }
                if let Some(alias) = alias {
                    self.notices.push(format!(
                        "USING {} is an alias of `quantized` and builds no new family (QL_CONTRACT §5 deviation 6): a symmetric int8 companion index with an f32 rerank, not a graph",
                        alias.to_ascii_lowercase()
                    ));
                }
                CompiledIndex::QuantizedVector { field: column }
            }
        };
        Ok(WritePlan::CreateIndex {
            collection: c,
            name,
            method,
        })
    }
}

/// A COLUMN RULE as a `CREATE TABLE` would have written it.
pub(super) fn written_rule(rule: &ColumnRule) -> String {
    let mut parts = Vec::new();
    match &rule.default {
        None => {}
        Some(DefaultValue::Now) => parts.push("DEFAULT now()".to_owned()),
        Some(DefaultValue::Uuid4) => parts.push("DEFAULT uuid4()".to_owned()),
        Some(DefaultValue::Uuid5 { name, .. }) => {
            parts.push(format!("DEFAULT uuid5(<namespace>, '{name}')"));
        }
    }
    if rule.not_null {
        parts.push("NOT NULL".to_owned());
    }
    parts.join(" ")
}
