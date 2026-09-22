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

    pub(super) fn create_table(
        &mut self,
        table: String,
        columns: Vec<ColumnDef>,
        if_not_exists: bool,
        with: &[WithIndex],
        automatic: &Automatic,
    ) -> SqlResult2<WritePlan> {
        // The catalog probe, run before anything is compiled: a table that is
        // already there is a NOTICE, not a refusal and not a second create.
        if if_not_exists && self.db.collection(&table)?.is_some() {
            return Ok(WritePlan::Notice(format!(
                "CREATE TABLE IF NOT EXISTS {table}: the collection is already in the catalog, so nothing was created -- and no AUTOMATIC index either, because a create that did not happen indexes nothing (docs/lang/INDEX_CONTRACT.md){}",
                if with.is_empty() {
                    String::new()
                } else {
                    // The clause is sugar for statements that follow the
                    // create, so a create that did not happen indexes
                    // nothing. Said out loud, because the whole point of the
                    // clause is that the caller knows what exists.
                    format!(
                        ", nor any index of the WITH clause; `SHOW INDEXES ON {table}` says what the collection already has"
                    )
                }
            )));
        }
        let mut fields = Vec::with_capacity(columns.len());
        let mut declared: Vec<(String, String)> = Vec::new();
        let mut rules: Vec<(String, ColumnRule)> = Vec::new();
        // `_key TEXT PRIMARY KEY` names the key every table already has, the
        // spelling e1 accepted and its README taught. It declares nothing new,
        // so it is read and dropped here, before the field list and the
        // automatic indexes see it: a second copy of the key is exactly what a
        // user-named PRIMARY KEY column costs, and this spelling asks for none.
        let names_builtin_key = |column: &ColumnDef| {
            column.name == "_key"
                && column.primary_key
                && matches!(column.kind, Kind::Text)
                && column.rule.is_none()
        };
        let builtin_key = columns.iter().any(names_builtin_key);
        let columns: Vec<ColumnDef> = columns
            .into_iter()
            .filter(|column| !names_builtin_key(column))
            .collect();
        let mut keys = usize::from(builtin_key);
        for column in &columns {
            if column.name.starts_with('_') {
                return Err(SqlError::unsupported(format!(
                    "column `{}`: names beginning with `_` are reserved (`_id`, `_key`, `_collection`). Every table already has `_key` as its primary key; `_key TEXT PRIMARY KEY` may name it, and nothing else may use the name.",
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
        if keys == 1 && !builtin_key {
            self.notices.push(format!(
                "PRIMARY KEY on `{table}`: the column is stored as a declared field AND supplies the external key `Database::put` maps, so the key is held twice (battle50k deviation 13)"
            ));
        }
        let declared_indexes = self.with_indexes(&table, &fields, with)?;
        let indexes = self.automatic_indexes(&table, &columns, automatic, declared_indexes)?;
        Ok(WritePlan::CreateTable {
            name: table,
            fields,
            declared,
            rules,
            indexes,
        })
    }

    /// The `CREATE TABLE ... WITH (...)` INDEX SUGAR of QL_CONTRACT §2,
    /// compiled to the `CREATE INDEX` list it stands for.
    ///
    /// It adds NO atomic: every pair becomes one of the `create_*_index`
    /// calls `CREATE INDEX` already compiles to, and the collection it
    /// indexes is the one `create_collection` of the same statement makes.
    /// What the sugar removes is the ceremony of writing the statements
    /// separately, not the EXPLICITNESS -- the caller still names every
    /// indexed column, and every mapping raises a NOTICE saying which family
    /// the key became, under which generated name, and why.
    ///
    /// Everything that can be decided is decided HERE, before the statement
    /// writes anything: an unknown key, a column the table does not declare,
    /// a family the column's `Kind` cannot carry, and a generated name that
    /// collides. That is what makes a refusal leave nothing behind.
    fn with_indexes(
        &mut self,
        table: &str,
        fields: &[(String, Kind)],
        with: &[WithIndex],
    ) -> SqlResult2<Vec<(String, CompiledIndex)>> {
        if with.is_empty() {
            return Ok(Vec::new());
        }
        let taken = self.every_index_name()?;
        let mut out: Vec<(String, CompiledIndex)> = Vec::new();
        for pair in with {
            let Some((_, kind)) = fields.iter().find(|(name, _)| *name == pair.column) else {
                return Err(SqlError::unsupported(format!(
                    "WITH ({}: [{}]): `{}` is not a column of `{table}`; the sugar indexes the columns the same statement declares, and nothing else",
                    pair.key, pair.column, pair.column
                )));
            };
            let (family, method, why) = with_family(&pair.key, &pair.column, kind)?;
            // The NAMING RULE, stated once: `<table>_<column>_<family>`,
            // where `<family>` is the family the key BECAME, not the key.
            // So `hash: [c]` and `range: [c]` generate one and the same name
            // -- they are one btree -- and writing both is a collision the
            // caller is told about rather than a second identical index.
            let name = format!("{table}_{}_{family}", pair.column);
            if out.iter().any(|(made, _)| *made == name) {
                return Err(SqlError::unsupported(format!(
                    "WITH ({}: [{}]): the generated index name `{name}` is already generated by an earlier pair of this same WITH clause -- `{family}` over `{}` is one index, and `hash` and `range` are both `btree`",
                    pair.key, pair.column, pair.column
                )));
            }
            if taken.iter().any(|held| *held == name) {
                return Err(SqlError::unsupported(format!(
                    "WITH ({}: [{}]): the generated index name `{name}` is already an index in this database. The sugar generates `<table>_<column>_<family>` and will not take a name that is held; drop it, or write this index by hand with a name of your own",
                    pair.key, pair.column
                )));
            }
            self.notices.push(format!(
                "WITH ({}: [{}]) on `{table}`: {why}, created as `{name}`",
                pair.key, pair.column
            ));
            out.push((name, method));
        }
        Ok(out)
    }

    /// The AUTOMATIC indexes of `docs/lang/INDEX_CONTRACT.md`, appended to
    /// whatever the `WITH (...)` sugar already declared.
    ///
    /// The rule that contract fixes in one line: **an index is DECLARED when
    /// there is a decision and AUTOMATIC when there is not.** A `SMALLINT`
    /// has one implementation, costs a few bytes per row, and nobody would
    /// ever choose differently, so asking for `CREATE INDEX` over it is
    /// ceremony with nothing behind it. `VECTOR(n)` is the opposite -- exact
    /// against quantized is recall against hundreds of gigabytes -- and stays
    /// declared, as does the `gin` text index, which brings an analyzer and
    /// an index the size of the corpus.
    ///
    /// This adds NO atomic and no family: each entry is one of the same
    /// `create_*_index` calls `CREATE INDEX` compiles to, built by the same
    /// `build_index` inside the same statement, under the SAME generated name
    /// `<table>_<column>_<family>` the sugar generates. That shared name is
    /// what makes an automatic index and a declared one for the same column
    /// ONE index rather than two: a `WITH (spatial: [loc])` entry and the
    /// automatic point index over `loc` collide on `t_loc_gist`, and the
    /// declared one -- which carries its own notice -- is kept.
    fn automatic_indexes(
        &mut self,
        table: &str,
        columns: &[ColumnDef],
        automatic: &Automatic,
        mut out: Vec<(String, CompiledIndex)>,
    ) -> SqlResult2<Vec<(String, CompiledIndex)>> {
        let wanted: Vec<&ColumnDef> = match automatic {
            Automatic::None => Vec::new(),
            Automatic::All => columns.iter().collect(),
            Automatic::Only(names) => {
                let mut picked = Vec::with_capacity(names.len());
                for name in names {
                    let Some(column) = columns.iter().find(|c| c.name == *name) else {
                        return Err(SqlError::unsupported(format!(
                            "WITH (index: [{name}]): `{name}` is not a column of `{table}`; the key names the columns of this same statement, and nothing else (docs/lang/INDEX_CONTRACT.md)"
                        )));
                    };
                    if automatic_index(&column.name, &column.kind).is_none() {
                        return Err(SqlError::unsupported(format!(
                            "WITH (index: [{name}]): `{name}` is declared {} and there is no family this engine gives it without being asked -- a VECTOR column is the one genuine trade (`exact` against `quantized`) and a JSONB column has no family at all. Declare the vector index you want; see docs/lang/INDEX_CONTRACT.md",
                            column.declared
                        )));
                    }
                    picked.push(column);
                }
                picked
            }
        };
        if wanted.is_empty() {
            return Ok(out);
        }
        let taken = self.every_index_name()?;
        let mut made: Vec<String> = Vec::new();
        for column in wanted {
            let Some((family, method)) = automatic_index(&column.name, &column.kind) else {
                continue;
            };
            let name = format!("{table}_{}_{family}", column.name);
            // The same name the sugar generated: one index, not two. The
            // declared entry stays, because it is the one the caller wrote
            // and the one whose notice already named the mapping.
            if out.iter().any(|(held, _)| *held == name) {
                continue;
            }
            // A generated name held ELSEWHERE in the database would make
            // `DROP INDEX <name>` ambiguous, which is why the sugar refuses
            // on it. An automatic index is not worth refusing a `CREATE
            // TABLE` over, so it is skipped and SAID -- the column is then
            // unindexed and its predicate is refused, naming it.
            if taken.iter().any(|held| *held == name) {
                self.notices.push(format!(
                    "automatic index on `{}`: the generated name `{name}` is already an index in this database, so it was NOT created and a predicate over `{}` is refused naming the column; write the index by hand with a name of your own",
                    column.name, column.name
                ));
                continue;
            }
            made.push(format!(
                "`{name}` ({family}) over `{}` {}",
                column.name, column.declared
            ));
            out.push((name, method));
        }
        if out.len() > sekejap_core::collections::MAX_INDEXES {
            return Err(SqlError::unsupported(format!(
                "CREATE TABLE {table}: the automatic indexes of docs/lang/INDEX_CONTRACT.md would be {} and a collection holds at most {} indexes. Name the columns you filter on -- `WITH (index: [a, b, ...])` -- or turn them off with `WITH (index: none)`",
                out.len(),
                sekejap_core::collections::MAX_INDEXES
            )));
        }
        if !made.is_empty() {
            // Nothing is created that the caller was not told about: the same
            // reason every `WITH` mapping and every key mapping says so.
            self.notices.push(format!(
                "CREATE TABLE {table}: {} automatic index(es) -- {} -- created with the collection and maintained on every write (docs/lang/INDEX_CONTRACT.md). `WITH (index: none)` creates none; `WITH (index: [column, ...])` creates only those",
                made.len(),
                made.join(", ")
            ));
        }
        Ok(out)
    }

    /// Every index name this database holds, across every collection.
    ///
    /// The check is DATABASE-wide rather than collection-wide because
    /// `DROP INDEX <name>` resolves a bare name over the whole catalog
    /// (`lang/src/compile/mod.rs::index_named`): a generated name that
    /// duplicates one held elsewhere would make that statement ambiguous.
    fn every_index_name(&self) -> SqlResult2<Vec<String>> {
        let mut out = Vec::new();
        for name in self.db.list_collections().map_err(SqlError::from)? {
            let Some(id) = self.db.collection(&name).map_err(SqlError::from)? else {
                continue;
            };
            for index in self.db.list_indexes(id).map_err(SqlError::from)? {
                out.push(index.name);
            }
        }
        Ok(out)
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
            Some(DefaultValue::Ulid) => self.notices.push(format!(
                "DEFAULT ulid() on `{column}`: a millisecond timestamp then eighty random bits, twenty-six Crockford base32 characters. It ASCENDS, so as a key it appends beside the last row instead of splitting pages across the tree the way a random uuid4() does, and it is ten bytes shorter"
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
        let mut create_indexes: Vec<(String, CompiledIndex)> = Vec::new();
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
                // The automatic index of `docs/lang/INDEX_CONTRACT.md`. A
                // column added by ALTER is a column, and a caller who cannot
                // filter on it until they write a second statement is in
                // exactly the position the contract removes. The build runs
                // over the rows already there, which is what makes the
                // predicate answer as soon as the statement returns.
                if let Some((family, method)) = automatic_index(&column.name, &column.kind) {
                    let name = format!("{table}_{}_{family}", column.name);
                    if self.every_index_name()?.iter().any(|held| *held == name) {
                        self.notices.push(format!(
                            "ADD COLUMN {}: the generated name `{name}` is already an index in this database, so no automatic index was created and a predicate over `{}` is refused naming the column",
                            column.name, column.name
                        ));
                    } else {
                        self.notices.push(format!(
                            "ADD COLUMN {}: one automatic index `{name}` ({family}) over `{}` {}, built over the rows already there (docs/lang/INDEX_CONTRACT.md)",
                            column.name, column.name, column.declared
                        ));
                        create_indexes.push((name, method));
                    }
                }
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
                // An index descriptor carries the field NAME, and there is
                // no atomic that rewrites it. A HAND-WRITTEN index is
                // therefore still a refusal naming it: the caller chose that
                // name and this statement will not silently take it away.
                //
                // An AUTOMATIC index is different, and only because the
                // engine made it: its name is the generated
                // `<table>_<column>_<family>`, nobody wrote it down, and the
                // collection is EMPTY -- the rename already refuses over a
                // populated one -- so the index over the old name holds no
                // entry. It is dropped and re-earned under the new name in
                // this same statement, which leaves the column exactly as
                // indexed as it was.
                let kind_of = fields
                    .iter()
                    .find(|(n, _)| n == from)
                    .map(|(_, k)| k.clone())
                    .expect("named() proved the column is there");
                let generated = automatic_index(from, &kind_of)
                    .map(|(family, _)| format!("{table}_{from}_{family}"));
                let held = over(from);
                if let Some((_, index)) = held
                    .iter()
                    .find(|(_, name)| generated.as_deref() != Some(name.as_str()))
                {
                    return Err(SqlError::unsupported(format!(
                        "RENAME COLUMN {from}: the index `{index}` names `{from}` in its own descriptor, and there is no atomic that rewrites an index descriptor's field; DROP INDEX {index} first and create it again on `{to}`"
                    )));
                }
                if !held.is_empty() {
                    drop_indexes = held;
                    if let Some((family, method)) = automatic_index(to, &kind_of) {
                        let name = format!("{table}_{to}_{family}");
                        self.notices.push(format!(
                            "RENAME COLUMN {from} TO {to}: the automatic index `{}` is dropped and re-earned as `{name}` in this same statement -- the collection is empty, which the rename already required, so the index holds no entry to move (docs/lang/INDEX_CONTRACT.md)",
                            drop_indexes[0].1
                        ));
                        create_indexes.push((name, method));
                    }
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
                create_indexes,
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
                        create_indexes,
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
                out.push_str(&format!(
                    "automatic: {}
",
                    if create_indexes.is_empty() {
                        "none created -- no column of this rewrite has a family it is given unasked".to_owned()
                    } else {
                        format!(
                            "created after the layout is repointed: {}",
                            create_indexes
                                .iter()
                                .map(|(name, _)| name.clone())
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
            IndexMethod::JsonBtree { column, member } => {
                if !matches!(self.kind_of(c, &column)?, Kind::Json) {
                    return Err(SqlError::unsupported(format!(
                        "({column}->>'{member}'): `->>` extracts from a JSONB column, and `{column}` is not one"
                    )));
                }
                self.notices.push(format!(
                    "an expression index over {column}->>'{member}' stores the TEXT at that member: a member that is absent, JSON null, an object or an array stores the NULL key and no equality can name it (docs/lang/INDEX_CONTRACT.md)"
                ));
                CompiledIndex::JsonScalar {
                    field: column,
                    member,
                }
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
            IndexMethod::Vamana { column, alias } => {
                if !matches!(self.kind_of(c, &column)?, Kind::Vector(_)) {
                    return Err(SqlError::unsupported(format!(
                        "vamana({column}): the vamana family indexes a VECTOR column"
                    )));
                }
                if let Some(alias) = alias {
                    self.notices.push(format!(
                        "USING {alias} builds the `vamana` family: the Vamana/DiskANN graph over quantized codes, with an f32 rerank"
                    ));
                }
                self.notices.push(
                    "a vamana index is maintained live on every write and is NOT READY until its build finishes; while it is building a vector order over this column is refused rather than answered from a partial graph (INDEX_CONTRACT)".into(),
                );
                CompiledIndex::VamanaGraph { field: column }
            }
        };
        // `docs/lang/INDEX_CONTRACT.md`: an automatic index and a
        // hand-written `CREATE INDEX` for the same column are ONE index, not
        // two. The generated name is `<table>_<column>_<family>` and the
        // caller's name is their own, so the two never collide on the name;
        // what decides is the DESCRIPTOR -- same family, same field, same
        // expression, same uniqueness. A second index over all four would be
        // a second copy of the same keys, maintained on every write, and no
        // statement asks for that on purpose.
        if let Some(held) = self
            .db
            .list_indexes(c)
            .map_err(SqlError::from)?
            .into_iter()
            .find(|info| same_index(&method, info))
        {
            return Ok(WritePlan::Notice(format!(
                "CREATE INDEX {name} ON {table}: `{}` already indexes `{}` in the same family, so nothing was created and `{name}` is not a name this database holds. Every eligible column is indexed when the table is created (docs/lang/INDEX_CONTRACT.md); `SHOW INDEXES ON {table}` says what is there",
                held.name, held.field
            )));
        }
        Ok(WritePlan::CreateIndex {
            collection: c,
            name,
            method,
        })
    }
}

/// Whether a compiled index and an index the catalog already holds are the
/// SAME index: one family, one field, one expression, one uniqueness.
///
/// The name is deliberately not part of it. Two names over one descriptor are
/// two copies of the same keys maintained on every write, which is the cost
/// `docs/lang/INDEX_CONTRACT.md` weighs and never the thing a caller wants.
fn same_index(method: &CompiledIndex, info: &IndexInfo) -> bool {
    match method {
        CompiledIndex::Scalar { field, unique } => {
            info.family == IndexFamily::Scalar
                && info.expression.is_none()
                && info.field == *field
                && info.unique == *unique
        }
        CompiledIndex::LowerScalar { field } => {
            info.family == IndexFamily::Scalar
                && info.expression == Some(IndexExpr::Lower)
                && info.field == *field
        }
        CompiledIndex::JsonScalar { field, member } => {
            info.family == IndexFamily::Scalar
                && info.expression == Some(IndexExpr::JsonText(member.clone()))
                && info.field == *field
        }
        CompiledIndex::Text { field } => info.family == IndexFamily::Text && info.field == *field,
        CompiledIndex::Point { field } => {
            info.family == IndexFamily::SpatialPoint && info.field == *field
        }
        CompiledIndex::Geometry { field } => {
            info.family == IndexFamily::SpatialGeometry && info.field == *field
        }
        CompiledIndex::ExactVector { field } => {
            info.family == IndexFamily::ExactVector && info.field == *field
        }
        CompiledIndex::QuantizedVector { field } => {
            info.family == IndexFamily::QuantizedVector && info.field == *field
        }
        CompiledIndex::VamanaGraph { field } => {
            info.family == IndexFamily::VamanaGraph && info.field == *field
        }
    }
}

/// One `WITH` key, against one column's declared `Kind`.
///
/// Returns the FAMILY word the generated name carries, the compiled index the
/// key stands for, and the sentence its notice says. The key set and the
/// mapping are QL_CONTRACT §2's, written out once here so that the notice,
/// the refusal and the generated name all read the same table:
///
/// | key | family | legal `Kind` |
/// | --- | --- | --- |
/// | `hash` | `btree` | Bool, Int, Real, Text |
///
/// The eighth key, `index:`, is not in this table because it names no family:
/// it says which columns get the AUTOMATIC index of
/// `docs/lang/INDEX_CONTRACT.md`, and it is handled in `automatic_indexes`.
/// | `range` | `btree` | Bool, Int, Real, Text |
/// | `fulltext` | `gin` | Text |
/// | `bm25` | `gin` | Text |
/// | `spatial` | `gist` | Point, Geo |
/// | `vector` | `exact` | Vector(n) |
/// | `quantized` | `quantized` | Vector(n) |
///
/// A key that is legal but whose column cannot carry it is REFUSED by name
/// with both the key and the `Kind`, because a `gist` over a TEXT column and
/// a `gin` over an INT column are not indexes this engine has; there is no
/// second-best family to fall back to, and falling back silently is the
/// silent scan §6 forbids.
fn with_family(
    key: &str,
    column: &str,
    kind: &Kind,
) -> SqlResult2<(&'static str, CompiledIndex, String)> {
    let refuse = |family: &str, wants: &str| -> SqlError {
        SqlError::unsupported(format!(
            "WITH ({key}: [{column}]): `{key}` is a `{family}` index and a `{family}` indexes {wants}; `{column}` is declared {kind:?}. The keys are index, hash, range, fulltext, bm25, spatial, vector, quantized (QL_CONTRACT §2)"
        ))
    };
    Ok(match key {
        // One scalar family answers equality AND range, so the two keys a
        // caller reaches for are one index. The notice says so rather than
        // letting the caller believe two different things were built.
        "hash" | "range" => {
            if !matches!(kind, Kind::Bool | Kind::Int | Kind::Real | Kind::Text) {
                return Err(refuse("btree", "a BOOLEAN, INT, REAL or TEXT column"));
            }
            let why = if key == "hash" {
                format!("`hash` became a `btree` over `{column}` -- there is no separate hash family here, and a `btree` answers equality")
            } else {
                format!("`range` became a `btree` over `{column}` -- one scalar family answers equality and range alike")
            };
            (
                "btree",
                CompiledIndex::Scalar {
                    field: column.to_owned(),
                    unique: false,
                },
                why,
            )
        }
        "fulltext" | "bm25" => {
            if !matches!(kind, Kind::Text) {
                return Err(refuse("gin", "a declared TEXT column"));
            }
            let why = if key == "fulltext" {
                format!("`fulltext` became a `gin` over `to_tsvector('simple', {column})` -- analyzer v1, one text index per declared TEXT field")
            } else {
                format!("`bm25` became a `gin` over `to_tsvector('simple', {column})` -- BM25 is how that one text index SCORES, not a family of its own")
            };
            (
                "gin",
                CompiledIndex::Text {
                    field: column.to_owned(),
                },
                why,
            )
        }
        "spatial" => match kind {
            Kind::Point => (
                "gist",
                CompiledIndex::Point {
                    field: column.to_owned(),
                },
                format!("`spatial` became a `gist` over `{column}` -- the POINT family, Hilbert postings and the ring walk a nearest order takes"),
            ),
            Kind::Geo => (
                "gist",
                CompiledIndex::Geometry {
                    field: column.to_owned(),
                },
                format!("`spatial` became a `gist` over `{column}` -- the GEOMETRY family, cell covers at three levels"),
            ),
            _ => return Err(refuse("gist", "a GEOMETRY(Point) or GEOMETRY(Polygon) column")),
        },
        "vector" => {
            if !matches!(kind, Kind::Vector(_)) {
                return Err(refuse("exact", "a VECTOR(n) column"));
            }
            (
                "exact",
                CompiledIndex::ExactVector {
                    field: column.to_owned(),
                },
                format!("`vector` became an `exact` vector index over `{column}` -- every vector is scored, so the answer is the true nearest set"),
            )
        }
        "quantized" => {
            if !matches!(kind, Kind::Vector(_)) {
                return Err(refuse("quantized", "a VECTOR(n) column"));
            }
            (
                "quantized",
                CompiledIndex::QuantizedVector {
                    field: column.to_owned(),
                },
                format!("`quantized` became a `quantized` vector index over `{column}` -- a symmetric int8 companion with an f32 rerank, so the answer is APPROXIMATE and says so"),
            )
        }
        other => {
            return Err(SqlError::unsupported(format!(
                "WITH ({other}: [{column}]): `{other}` is not an index key. The keys are index, hash, range, fulltext, bm25, spatial, vector, quantized (QL_CONTRACT §2)"
            )))
        }
    })
}

/// The index `docs/lang/INDEX_CONTRACT.md` gives a column of this `Kind`
/// WITHOUT being asked, and the family word its generated name carries.
///
/// `None` is the contract's right-hand column: `VECTOR(n)`, where exactness
/// against size is the one genuine trade in the system, and `JSONB`, which
/// has no family at all and is a stated gap rather than a decision.
///
/// The scalar family already accepts exactly bool/int/real/text
/// (`core/engine/src/collections/catalog.rs::kind_byte`) and the two spatial
/// families already follow the declared shape, so this introduces no family,
/// no keyspace and no feature bit -- it only decides who gets one unasked.
/// The family words match `with_family`'s, so a `hash`/`range` entry and the
/// automatic scalar index generate ONE name, and `spatial` and the automatic
/// point or geometry index generate one name too.
pub(super) fn automatic_index(
    column: &str,
    kind: &Kind,
) -> Option<(&'static str, CompiledIndex)> {
    match kind {
        Kind::Bool | Kind::Int | Kind::Real | Kind::Text => Some((
            "btree",
            CompiledIndex::Scalar {
                field: column.to_owned(),
                unique: false,
            },
        )),
        Kind::Point => Some((
            "gist",
            CompiledIndex::Point {
                field: column.to_owned(),
            },
        )),
        Kind::Geo => Some((
            "gist",
            CompiledIndex::Geometry {
                field: column.to_owned(),
            },
        )),
        Kind::Json | Kind::Vector(_) => None,
    }
}

/// A COLUMN RULE as a `CREATE TABLE` would have written it.
pub(super) fn written_rule(rule: &ColumnRule) -> String {
    let mut parts = Vec::new();
    match &rule.default {
        None => {}
        Some(DefaultValue::Now) => parts.push("DEFAULT now()".to_owned()),
        Some(DefaultValue::Uuid4) => parts.push("DEFAULT uuid4()".to_owned()),
        Some(DefaultValue::Ulid) => parts.push("DEFAULT ulid()".to_owned()),
        Some(DefaultValue::Uuid5 { name, .. }) => {
            parts.push(format!("DEFAULT uuid5(<namespace>, '{name}')"));
        }
    }
    if rule.not_null {
        parts.push("NOT NULL".to_owned());
    }
    parts.join(" ")
}
